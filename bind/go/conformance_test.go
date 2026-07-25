package slate_test

import (
	"bytes"
	"errors"
	"os"
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
