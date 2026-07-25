//go:build darwin

package slate

/*
#cgo LDFLAGS: ${SRCDIR}/../../target/release/libslate_kv_ffi.a -liconv -lSystem -lc -lm
*/
import "C"
