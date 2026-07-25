//go:build linux

package slate

/*
#cgo LDFLAGS: -L${SRCDIR}/../../target/release -L../../target/release -lslate_kv_ffi -lgcc_s -lutil -lrt -lpthread -lm -ldl -lc
*/
import "C"
