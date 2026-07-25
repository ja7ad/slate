//go:build darwin

package slate

/*
#cgo LDFLAGS: -L${SRCDIR}/../../target/release -L../../target/release -lslate_kv_ffi -liconv -lSystem -lc -lm
*/
import "C"
