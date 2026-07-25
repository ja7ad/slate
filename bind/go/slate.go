// Package slate provides the Go language binding for the SLATE secure key-value engine.
// It acts as a lightweight translation layer over the C ABI, exposing idiomatic Go types,
// thread-safe operations, and strongly typed security errors.
package slate

/*
#cgo CFLAGS: -I${SRCDIR}/../../crates/slate-kv-ffi/include
#cgo darwin LDFLAGS: ${SRCDIR}/../../target/release/libslate_kv_ffi.a -liconv -lSystem -lc -lm
#cgo linux LDFLAGS: ${SRCDIR}/../../target/release/libslate_kv_ffi.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
#include "slate.h"
#include <stdlib.h>
*/
import "C"
import (
	"errors"
	"fmt"
	"sync"
	"unsafe"
)

const (
	CodeOk             = int32(C.SLATE_OK)
	CodeNotFound       = int32(C.SLATE_ERR_NOT_FOUND)
	CodeBufferTooSmall = int32(C.SLATE_ERR_BUFFER_TOO_SMALL)
	CodeInvalidArg     = int32(C.SLATE_ERR_INVALID_ARG)
	CodeTampered       = int32(C.SLATE_ERR_TAMPERED)
	CodeRollback       = int32(C.SLATE_ERR_ROLLBACK)
	CodeInternal       = int32(C.SLATE_ERR_INTERNAL)
	CodeIo             = int32(C.SLATE_ERR_IO)

	AbiVersionMajor = int(C.SLATE_ABI_VERSION_MAJOR)
	AbiVersionMinor = int(C.SLATE_ABI_VERSION_MINOR)
)

var (
	// ErrKeyNotFound is returned when the requested key is not in the database.
	ErrKeyNotFound = errors.New("slate: key not found")
	// ErrClosed is returned when an operation is attempted on a closed database handle.
	ErrClosed = errors.New("slate: database is closed")
	// ErrBufferTooSmall is returned by GetInto when the destination buffer is smaller than the value length.
	ErrBufferTooSmall = errors.New("slate: buffer too small")

	emptySentinel = []byte{0}
)

// TamperedError indicates that cryptographic verification of the database state failed.
// This indicates potential storage modification or corruption and should never be retried or ignored.
type TamperedError struct {
	Msg string
}

func (e *TamperedError) Error() string {
	if e.Msg != "" {
		return fmt.Sprintf("slate: tampered (%s)", e.Msg)
	}
	return "slate: tampered"
}

// RollbackError indicates an attempted stale-epoch or rollback attack on the database.
// This indicates potential storage rollback and should never be retried or ignored.
type RollbackError struct {
	Msg string
}

func (e *RollbackError) Error() string {
	if e.Msg != "" {
		return fmt.Sprintf("slate: rollback detected (%s)", e.Msg)
	}
	return "slate: rollback detected"
}

// SlateError is a general operational error returned by the engine.
type SlateError struct {
	Code int32
	Msg  string
}

func (e *SlateError) Error() string {
	if e.Msg != "" {
		return fmt.Sprintf("slate error %d: %s", e.Code, e.Msg)
	}
	return fmt.Sprintf("slate error %d", e.Code)
}

// SecurityMode describes the level of rollback and tamper protection active on the engine.
type SecurityMode int32

const (
	// SecurityModeFull guarantees full rollback protection and tamper resistance using hardware eFuses.
	SecurityModeFull SecurityMode = 0
	// SecurityModeBestEffortRollback provides tamper resistance and best-effort rollback protection using flash storage. Note that rollback protection is degraded compared to hardware counters.
	SecurityModeBestEffortRollback SecurityMode = 1
	// SecurityModeNoRollbackProtection provides tamper resistance but no rollback protection. Note that rollback protection is severely degraded.
	SecurityModeNoRollbackProtection SecurityMode = 2
)

// Profile selects the tuning parameter defaults for the engine.
type Profile uint8

// Profile values come from slate.h, never from a hand-written literal: the ABI
// mapping is 0 = Pi, 1 = ESP32, and inverting it silently selects the wrong
// commit-batch operating point (report §9: Pi B = 9, ESP32 B = 27).
const (
	// ProfilePi tunes the commit scheduler for a Raspberry-Pi-class host.
	ProfilePi Profile = Profile(C.SLATE_PROFILE_PI)
	// ProfileEsp32 tunes the commit scheduler for an ESP32-class device.
	ProfileEsp32 Profile = Profile(C.SLATE_PROFILE_ESP32)
)

// Options configures the database upon opening.
type Options struct {
	CapacityBytes uint64
	MaxKeys       uint32
	BCommit       uint32
	Theta         uint32
	Profile       Profile
}

// DB represents an open database handle.
// All operations are synchronized under a mutex and verify handle liveness before executing.
type DB struct {
	mu     sync.Mutex
	ptr    *C.slate_db
	closed bool
}

func slicePtr(s []byte) *C.uint8_t {
	if len(s) == 0 {
		return (*C.uint8_t)(&emptySentinel[0])
	}
	return (*C.uint8_t)(&s[0])
}

func lastErrorMessage(ptr *C.slate_db) string {
	var buf [512]C.char
	n := C.slate_last_error_message(ptr, &buf[0], C.uintptr_t(len(buf)))
	if n == 0 {
		return ""
	}
	return C.GoString(&buf[0])
}

func (db *DB) mapErrorLocked(rc int32) error {
	if rc == CodeOk {
		return nil
	}
	if rc == CodeNotFound {
		return ErrKeyNotFound
	}
	msg := lastErrorMessage(db.ptr)
	if rc == CodeTampered {
		return &TamperedError{Msg: msg}
	}
	if rc == CodeRollback {
		return &RollbackError{Msg: msg}
	}
	if rc == CodeInvalidArg {
		return fmt.Errorf("slate invalid argument: %s", msg)
	}
	if rc == CodeIo {
		return fmt.Errorf("slate IO error: %s", msg)
	}
	return &SlateError{Code: rc, Msg: msg}
}

// Open opens or creates a database at the specified path using the provided 32-byte root key.
// The root key buffer copied internally is zeroed immediately upon returning from the C interface.
func Open(path string, rootKey []byte, opts *Options) (*DB, error) {
	if len(rootKey) != 32 {
		return nil, errors.New("slate: root key must be exactly 32 bytes")
	}

	ver := uint32(C.slate_abi_version())
	major := int(ver >> 16)
	if major != AbiVersionMajor {
		return nil, fmt.Errorf("slate ABI version major mismatch: expected %d, got %d", AbiVersionMajor, major)
	}

	if opts == nil {
		opts = &Options{
			CapacityBytes: 1024 * 1024,
			MaxKeys:       100,
			BCommit:       1,
			Theta:         0,
			Profile:       ProfilePi,
		}
	}

	cOpts := C.slate_options{
		capacity_bytes: C.uint64_t(opts.CapacityBytes),
		max_keys:       C.uint32_t(opts.MaxKeys),
		b_commit:       C.uint32_t(opts.BCommit),
		theta:          C.uint32_t(opts.Theta),
		profile:        C.uint8_t(opts.Profile),
	}

	// Copy and zero out the temporary key buffer to avoid keeping sensitive material in host memory.
	var keyBuf [32]byte
	copy(keyBuf[:], rootKey)
	defer func() {
		for i := range keyBuf {
			keyBuf[i] = 0
		}
	}()

	cPath := C.CString(path)
	defer C.free(unsafe.Pointer(cPath))

	var ptr *C.slate_db
	rc := int32(C.slate_open(cPath, (*C.uint8_t)(&keyBuf[0]), &cOpts, &ptr))
	if rc != CodeOk {
		msg := lastErrorMessage(nil)
		if rc == CodeTampered {
			return nil, &TamperedError{Msg: msg}
		}
		if rc == CodeRollback {
			return nil, &RollbackError{Msg: msg}
		}
		if rc == CodeInvalidArg {
			return nil, fmt.Errorf("slate open invalid argument: %s", msg)
		}
		if rc == CodeIo {
			return nil, fmt.Errorf("slate open IO error: %s", msg)
		}
		return nil, &SlateError{Code: rc, Msg: msg}
	}

	return &DB{ptr: ptr}, nil
}

// Close explicitly releases the database handle and underlying resources.
// Repeated calls or subsequent operations on a closed handle will safely return ErrClosed.
func (db *DB) Close() error {
	db.mu.Lock()
	defer db.mu.Unlock()
	if db.closed {
		return ErrClosed
	}
	db.closed = true
	C.slate_close(db.ptr)
	db.ptr = nil
	return nil
}

// Put writes a key-value pair to the in-memory write buffer.
func (db *DB) Put(key, val []byte) error {
	if len(key) == 0 {
		return errors.New("slate: key must not be empty")
	}
	db.mu.Lock()
	defer db.mu.Unlock()
	if db.closed {
		return ErrClosed
	}
	rc := int32(C.slate_put(db.ptr, slicePtr(key), C.uintptr_t(len(key)), slicePtr(val), C.uintptr_t(len(val))))
	return db.mapErrorLocked(rc)
}

// PutDurable writes a key-value pair to flash storage and blocks until durably committed.
func (db *DB) PutDurable(key, val []byte) error {
	if len(key) == 0 {
		return errors.New("slate: key must not be empty")
	}
	db.mu.Lock()
	defer db.mu.Unlock()
	if db.closed {
		return ErrClosed
	}
	rc := int32(C.slate_put_durable(db.ptr, slicePtr(key), C.uintptr_t(len(key)), slicePtr(val), C.uintptr_t(len(val))))
	return db.mapErrorLocked(rc)
}

// getIntoLocked issues a single slate_get call. The caller must hold db.mu and
// must already have checked db.closed.
func (db *DB) getIntoLocked(key, dst []byte) (int, error) {
	vlen := C.uintptr_t(len(dst))
	rc := int32(C.slate_get(db.ptr, slicePtr(key), C.uintptr_t(len(key)), slicePtr(dst), &vlen))
	if rc == CodeBufferTooSmall {
		return int(vlen), ErrBufferTooSmall
	}
	if rc != CodeOk {
		return 0, db.mapErrorLocked(rc)
	}
	return int(vlen), nil
}

// GetInto reads the value for a key into the provided destination buffer.
// If the buffer is too small, it returns the required byte length and ErrBufferTooSmall.
func (db *DB) GetInto(key, dst []byte) (int, error) {
	if len(key) == 0 {
		return 0, errors.New("slate: key must not be empty")
	}
	db.mu.Lock()
	defer db.mu.Unlock()
	if db.closed {
		return 0, ErrClosed
	}
	return db.getIntoLocked(key, dst)
}

// Get retrieves the value associated with the specified key.
// The size query and the read are issued under one lock, so a concurrent writer
// cannot grow the value in between and turn the second call into a short read.
func (db *DB) Get(key []byte) ([]byte, error) {
	if len(key) == 0 {
		return nil, errors.New("slate: key must not be empty")
	}
	db.mu.Lock()
	defer db.mu.Unlock()
	if db.closed {
		return nil, ErrClosed
	}

	var stack [256]byte
	n, err := db.getIntoLocked(key, stack[:])
	if err == nil {
		val := make([]byte, n)
		copy(val, stack[:n])
		return val, nil
	}
	if !errors.Is(err, ErrBufferTooSmall) {
		return nil, err
	}

	val := make([]byte, n)
	n, err = db.getIntoLocked(key, val)
	if err != nil {
		return nil, err
	}
	return val[:n], nil
}

// Delete marks a key as deleted.
func (db *DB) Delete(key []byte) error {
	if len(key) == 0 {
		return errors.New("slate: key must not be empty")
	}
	db.mu.Lock()
	defer db.mu.Unlock()
	if db.closed {
		return ErrClosed
	}
	rc := int32(C.slate_delete(db.ptr, slicePtr(key), C.uintptr_t(len(key))))
	return db.mapErrorLocked(rc)
}

// Commit flushes buffered writes to storage.
func (db *DB) Commit() error {
	db.mu.Lock()
	defer db.mu.Unlock()
	if db.closed {
		return ErrClosed
	}
	rc := int32(C.slate_commit(db.ptr))
	return db.mapErrorLocked(rc)
}

// Compact triggers garbage collection and segment compaction on the storage medium.
func (db *DB) Compact() error {
	db.mu.Lock()
	defer db.mu.Unlock()
	if db.closed {
		return ErrClosed
	}
	rc := int32(C.slate_compact(db.ptr))
	return db.mapErrorLocked(rc)
}

// SecurityMode returns the active security mode and rollback protection capability of the engine.
func (db *DB) SecurityMode() (SecurityMode, error) {
	db.mu.Lock()
	defer db.mu.Unlock()
	if db.closed {
		return 0, ErrClosed
	}
	// slate_security_mode reports failure as -1, which is also the numeric value
	// of SLATE_ERR_NOT_FOUND. Routing it through mapErrorLocked would report a
	// security-mode failure as "key not found", so it is mapped explicitly here.
	mode := int32(C.slate_security_mode(db.ptr))
	if mode < 0 {
		return 0, &SlateError{Code: CodeInternal, Msg: "security mode query failed"}
	}
	return SecurityMode(mode), nil
}
