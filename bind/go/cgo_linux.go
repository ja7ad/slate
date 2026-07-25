//go:build linux

package slate

/*
#cgo LDFLAGS: ${SRCDIR}/../../target/release/libslate_kv_ffi.a -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
*/
import "C"
