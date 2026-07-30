package slate_test

import (
	"bytes"
	"errors"
	"os"
	"runtime"
	"path/filepath"
	"strings"
	"testing"

	"github.com/ja7ad/slate/bind/go"
)

func rootKey() []byte {
	return bytes.Repeat([]byte{0x42}, 32)
}

func openFresh(t *testing.T, dir string) *slate.DB {
	t.Helper()
	opts := &slate.Options{
		CapacityBytes: 1024 * 1024,
		MaxKeys:       100,
		BCommit:       1,
		Theta:         0,
		Profile:       slate.ProfilePi,
	}
	db, err := slate.Open(dir, rootKey(), opts)
	if err != nil {
		t.Fatalf("slate.Open failed: %v", err)
	}
	if db == nil {
		t.Fatalf("slate.Open returned nil handle without error")
	}
	return db
}

// Test opening a fresh database directory with a 32-byte root key.
func TestC1(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)
	defer db.Close()
}

// Test writing a durable value and reading it back to verify equality.
func TestC2(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)
	defer db.Close()

	key := []byte("mykey")
	val := []byte("myvalue_data")

	if err := db.PutDurable(key, val); err != nil {
		t.Fatalf("PutDurable failed: %v", err)
	}

	got, err := db.Get(key)
	if err != nil {
		t.Fatalf("Get failed: %v", err)
	}
	if !bytes.Equal(got, val) {
		t.Fatalf("value mismatch: got %q, want %q", got, val)
	}
}

// Test retrieving an absent key returns key not found error.
func TestC3(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)
	defer db.Close()

	_, err := db.Get([]byte("absent_key"))
	if !errors.Is(err, slate.ErrKeyNotFound) {
		t.Fatalf("expected ErrKeyNotFound, got %v", err)
	}
}

// Test reading into an empty destination buffer returns needed length and buffer too small error.
func TestC4(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)
	defer db.Close()

	key := []byte("size_key")
	val := []byte("1234567890")
	if err := db.PutDurable(key, val); err != nil {
		t.Fatalf("PutDurable failed: %v", err)
	}

	n, err := db.GetInto(key, nil)
	if !errors.Is(err, slate.ErrBufferTooSmall) {
		t.Fatalf("expected ErrBufferTooSmall with nil dst, got %v", err)
	}
	if n != len(val) {
		t.Fatalf("expected needed length %d, got %d", len(val), n)
	}

	dst := make([]byte, n)
	n2, err := db.GetInto(key, dst)
	if err != nil {
		t.Fatalf("GetInto exact size failed: %v", err)
	}
	if n2 != len(val) || !bytes.Equal(dst, val) {
		t.Fatalf("round-trip mismatch: %q vs %q", dst, val)
	}
}

// Test deleting a key causes subsequent lookups to report not found.
func TestC5(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)
	defer db.Close()

	key := []byte("del_key")
	val := []byte("val")
	if err := db.PutDurable(key, val); err != nil {
		t.Fatalf("PutDurable failed: %v", err)
	}

	if err := db.Delete(key); err != nil {
		t.Fatalf("Delete failed: %v", err)
	}

	_, err := db.Get(key)
	if !errors.Is(err, slate.ErrKeyNotFound) {
		t.Fatalf("expected ErrKeyNotFound after Delete, got %v", err)
	}
}

// Test buffering a write, committing, closing, and reopening preserves the value.
func TestC6(t *testing.T) {
	dir := t.TempDir()
	opts := &slate.Options{
		CapacityBytes: 1024 * 1024,
		MaxKeys:       100,
		BCommit:       1,
		Theta:         0,
		Profile:       slate.ProfilePi,
	}

	db, err := slate.Open(dir, rootKey(), opts)
	if err != nil {
		t.Fatalf("Open 1 failed: %v", err)
	}

	key := []byte("buf_key")
	val := []byte("buf_val")
	if err := db.Put(key, val); err != nil {
		t.Fatalf("Put failed: %v", err)
	}
	if err := db.Commit(); err != nil {
		t.Fatalf("Commit failed: %v", err)
	}
	if err := db.Close(); err != nil {
		t.Fatalf("Close 1 failed: %v", err)
	}

	db2, err := slate.Open(dir, rootKey(), opts)
	if err != nil {
		t.Fatalf("Open 2 failed: %v", err)
	}
	defer db2.Close()

	got, err := db2.Get(key)
	if err != nil {
		t.Fatalf("Get after reopen failed: %v", err)
	}
	if !bytes.Equal(got, val) {
		t.Fatalf("mismatch after reopen: %q vs %q", got, val)
	}
}

// Test putting and getting an empty zero-length value round-trips correctly without pointer issues.
func TestC7(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)
	defer db.Close()

	key := []byte("empty_val_key")
	if err := db.PutDurable(key, nil); err != nil {
		t.Fatalf("PutDurable empty val failed: %v", err)
	}

	got, err := db.Get(key)
	if err != nil {
		t.Fatalf("Get empty val failed: %v", err)
	}
	if len(got) != 0 {
		t.Fatalf("expected 0-length slice, got len=%d", len(got))
	}
}

// Test simulating storage modification by flipping bytes in the counter file produces a distinct tamper error.
func TestC8(t *testing.T) {
	dir := t.TempDir()
	opts := &slate.Options{
		CapacityBytes: 1024 * 1024,
		MaxKeys:       100,
		BCommit:       1,
		Theta:         0,
		Profile:       slate.ProfilePi,
	}

	db, err := slate.Open(dir, rootKey(), opts)
	if err != nil {
		t.Fatalf("initial Open failed: %v", err)
	}
	if err := db.PutDurable([]byte("k"), []byte("v")); err != nil {
		t.Fatalf("PutDurable failed: %v", err)
	}
	if err := db.Close(); err != nil {
		t.Fatalf("Close failed: %v", err)
	}

	counterPath := filepath.Join(dir, "counter.bin")
	data, err := os.ReadFile(counterPath)
	if err != nil {
		t.Fatalf("ReadFile counter.bin failed: %v", err)
	}
	if len(data) < 41 {
		t.Fatalf("counter.bin too small: %d bytes", len(data))
	}
	data[0] = 0xFF
	data[40] = 0xFF
	if err := os.WriteFile(counterPath, data, 0644); err != nil {
		t.Fatalf("WriteFile counter.bin failed: %v", err)
	}

	_, err = slate.Open(dir, rootKey(), opts)
	if err == nil {
		t.Fatalf("expected error on tampered reopen, got nil")
	}

	var tamperedErr *slate.TamperedError
	if !errors.As(err, &tamperedErr) {
		t.Fatalf("expected *slate.TamperedError, got %T: %v", err, err)
	}
}

// Test query for active security mode on file-backed storage returns best-effort rollback mode.
func TestC9(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)
	defer db.Close()

	mode, err := db.SecurityMode()
	if err != nil {
		t.Fatalf("SecurityMode failed: %v", err)
	}
	if mode != slate.SecurityModeBestEffortRollback {
		t.Fatalf("expected BestEffortRollback mode, got %v", mode)
	}
}

// Test closing handle repeatedly and operating after close safely returns closed database error.
func TestC10(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)

	if err := db.Close(); err != nil {
		t.Fatalf("first Close failed: %v", err)
	}

	if err := db.Close(); !errors.Is(err, slate.ErrClosed) {
		t.Fatalf("expected ErrClosed on second Close, got %v", err)
	}

	if err := db.Put([]byte("k"), []byte("v")); !errors.Is(err, slate.ErrClosed) {
		t.Fatalf("expected ErrClosed on Put after Close, got %v", err)
	}

	if _, err := db.Get([]byte("k")); !errors.Is(err, slate.ErrClosed) {
		t.Fatalf("expected ErrClosed on Get after Close, got %v", err)
	}
}

// Test failing operations and invalid open parameters return descriptive error messages.
func TestC11(t *testing.T) {
	dir := t.TempDir()

	db := openFresh(t, dir)
	defer db.Close()

	err := db.Put(nil, []byte("v"))
	if err == nil || err.Error() == "" {
		t.Fatalf("expected non-empty error message for empty key Put, got %v", err)
	}

	opts := &slate.Options{
		CapacityBytes: 0xFFFFFFFF + 10,
		MaxKeys:       100,
		BCommit:       1,
		Theta:         0,
		Profile:       slate.ProfilePi,
	}
	_, err = slate.Open(dir, rootKey(), opts)
	if err == nil {
		t.Fatalf("expected error for oversized capacity, got nil")
	}
	// The message must have come back through slate_last_error_message(NULL, ...):
	// a failed open yields no handle, so this is the only channel for it. Asserting
	// only that err.Error() != "" would pass on the Go-side format string alone.
	if !strings.Contains(err.Error(), "capacity_bytes") {
		t.Fatalf("expected the FFI open-time message to reach the caller, got %q", err.Error())
	}
}

// Test that the profile selectors carry the ABI's numeric values (0 = Pi, 1 = ESP32).
// Inverting them silently selects the wrong commit-batch operating point.
func TestProfileConstantsMatchABI(t *testing.T) {
	if slate.ProfilePi != 0 {
		t.Fatalf("ProfilePi must be 0, got %d", slate.ProfilePi)
	}
	if slate.ProfileEsp32 != 1 {
		t.Fatalf("ProfileEsp32 must be 1, got %d", slate.ProfileEsp32)
	}
}

// Test passing invalid root key length is rejected before entering native interface.
func TestC12(t *testing.T) {
	dir := t.TempDir()
	opts := &slate.Options{
		CapacityBytes: 1024 * 1024,
		MaxKeys:       100,
		BCommit:       1,
		Theta:         0,
		Profile:       slate.ProfilePi,
	}

	shortKey := []byte{1, 2, 3}
	_, err := slate.Open(dir, shortKey, opts)
	if err == nil {
		t.Fatalf("expected error for 3-byte key, got nil")
	}
}

// Test verifying the major version of the ABI matches expectations.
func TestC13(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)
	defer db.Close()
	if slate.AbiVersionMajor != 1 {
		t.Fatalf("expected AbiVersionMajor == 1, got %d", slate.AbiVersionMajor)
	}
}

// ---------------------------------------------------------------------------
// Environmental cases (C14-C18). See bind/CONFORMANCE.md.
//
// C1-C13 assume the library loaded and linked correctly. These five test that
// assumption, and each exists because a real failure slipped past the
// behavioural cases.
// ---------------------------------------------------------------------------

// C14: the engine must survive being called from the runtime's smallest unit
// of concurrency, not just the main thread.
//
// slate_open needs >= 52 KiB of stack (measured: SIGSEGV at 48 KiB, clean at
// 52 KiB). Go grows goroutine stacks on demand so this is free here, but
// TinyGo and other runtimes give a fixed, much smaller stack -- and the
// failure mode is a bare SIGSEGV on the first engine call with no other
// symptom. Running it explicitly on a goroutine makes that a named test
// failure instead of a mystery crash in whichever case happens to run first.
func TestC14(t *testing.T) {
	dir := t.TempDir()
	done := make(chan error, 1)
	go func() {
		db, err := slate.Open(dir, rootKey(), &slate.Options{
			CapacityBytes: 1024 * 1024,
			MaxKeys:       100,
			BCommit:       1,
			Theta:         0,
			Profile:       slate.ProfilePi,
		})
		if err != nil {
			done <- err
			return
		}
		if err := db.PutDurable([]byte("k"), []byte("v")); err != nil {
			done <- err
			return
		}
		done <- db.Close()
	}()
	if err := <-done; err != nil {
		t.Fatalf("engine call on a goroutine failed: %v", err)
	}
}

// C15: a failed open's message must be readable from a DIFFERENT thread than
// the one that failed.
//
// Nothing in the C ABI promises thread affinity. This fails against a
// thread-local error slot, which returns an empty string to the reader --
// which is why the FFI uses a process-wide mutex for it.
func TestC15(t *testing.T) {
	dir := t.TempDir()

	// Fail an open on a dedicated goroutine, locked to its own OS thread so
	// the failure and the read genuinely happen on different threads.
	failed := make(chan error, 1)
	go func() {
		runtime.LockOSThread()
		defer runtime.UnlockOSThread()
		_, err := slate.Open(dir, rootKey(), &slate.Options{
			CapacityBytes: 0xFFFFFFFF + 10, // rejected by slate_open
			MaxKeys:       100,
			BCommit:       1,
			Theta:         0,
			Profile:       slate.ProfilePi,
		})
		failed <- err
	}()
	err := <-failed
	if err == nil {
		t.Fatal("expected the oversized-capacity open to fail")
	}
	// The message must have survived the thread hop.
	if !strings.Contains(err.Error(), "capacity_bytes") {
		t.Fatalf("error did not carry the native message across threads: %v", err)
	}
}

// C16: the three-step buffer protocol -- size query, short buffer, exact fit.
func TestC16(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)
	defer db.Close()

	val := bytes.Repeat([]byte{0xAB}, 300)
	if err := db.PutDurable([]byte("bufkey"), val); err != nil {
		t.Fatalf("PutDurable failed: %v", err)
	}

	// Zero-capacity: a pure size query.
	n, err := db.GetInto([]byte("bufkey"), nil)
	if err == nil {
		t.Fatal("expected buffer-too-small for a zero-capacity read")
	}
	if !errors.Is(err, slate.ErrBufferTooSmall) {
		t.Fatalf("expected ErrBufferTooSmall, got %v", err)
	}
	if n != len(val) {
		t.Fatalf("size query reported %d, want %d", n, len(val))
	}

	// One byte short: must refuse and must not scribble past the buffer.
	short := make([]byte, len(val)-1)
	for i := range short {
		short[i] = 0x5A
	}
	if _, err := db.GetInto([]byte("bufkey"), short); !errors.Is(err, slate.ErrBufferTooSmall) {
		t.Fatalf("expected ErrBufferTooSmall for a short buffer, got %v", err)
	}
	for i, b := range short {
		if b != 0x5A {
			t.Fatalf("short buffer was modified at index %d (0x%02X)", i, b)
		}
	}

	// Exact fit: succeeds.
	exact := make([]byte, len(val))
	got, err := db.GetInto([]byte("bufkey"), exact)
	if err != nil {
		t.Fatalf("exact-size GetInto failed: %v", err)
	}
	if got != len(val) || !bytes.Equal(exact, val) {
		t.Fatalf("exact-size read returned %d bytes, mismatch=%v", got, !bytes.Equal(exact, val))
	}
}

// C17: binary (non-UTF-8) values and a long key must round-trip byte-for-byte.
// Catches bindings that treat values as text or truncate at an embedded NUL.
func TestC17(t *testing.T) {
	dir := t.TempDir()
	db := openFresh(t, dir)
	defer db.Close()

	// Every byte value, including 0x00, four times over.
	binary := make([]byte, 0, 1024)
	for i := 0; i < 4; i++ {
		for b := 0; b < 256; b++ {
			binary = append(binary, byte(b))
		}
	}
	if err := db.PutDurable([]byte("binary"), binary); err != nil {
		t.Fatalf("PutDurable(binary) failed: %v", err)
	}
	got, err := db.Get([]byte("binary"))
	if err != nil {
		t.Fatalf("Get(binary) failed: %v", err)
	}
	if !bytes.Equal(got, binary) {
		t.Fatalf("binary value did not round-trip: got %d bytes, want %d", len(got), len(binary))
	}

	longKey := bytes.Repeat([]byte("k"), 64)
	if err := db.PutDurable(longKey, []byte("lk")); err != nil {
		t.Fatalf("PutDurable(long key) failed: %v", err)
	}
	if got, err := db.Get(longKey); err != nil || !bytes.Equal(got, []byte("lk")) {
		t.Fatalf("long key did not round-trip: %v / %q", err, got)
	}
}

// C18: the profile constants must match slate.h.
//
// Swapping them does not error -- it silently selects the wrong flash
// geometry, which is the worst kind of bug to find in production.
func TestC18(t *testing.T) {
	if slate.ProfilePi != 0 {
		t.Fatalf("ProfilePi must be 0 per slate.h, got %d", slate.ProfilePi)
	}
	if slate.ProfileEsp32 != 1 {
		t.Fatalf("ProfileEsp32 must be 1 per slate.h, got %d", slate.ProfileEsp32)
	}
}
