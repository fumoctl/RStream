package main

/*
#include <stdlib.h>

struct RcloneRPCResult {
	char*	Output;
	int	Status;
};

typedef int (*RcloneOpenURLFn)(const char* url);

static inline int call_open_url(RcloneOpenURLFn fn, const char* url) {
	if (!fn) return -1;
	return fn(url);
}
*/
import "C"

import (
	"errors"
	"os"
	"runtime"
	"sync"
	"unsafe"

	"github.com/rclone/rclone/lib/oauthutil"
	"github.com/rclone/rclone/librclone/librclone"

	_ "github.com/rclone/rclone/backend/all"
	_ "github.com/rclone/rclone/cmd/serve/http"
	_ "github.com/rclone/rclone/fs/operations"
	_ "github.com/rclone/rclone/fs/sync"
)

var (
	defaultOpenURL = oauthutil.OpenURL
	openURLMu      sync.RWMutex
	openURLFn      C.RcloneOpenURLFn
)

func init() {
	if runtime.GOOS == "android" && os.Getenv("SSL_CERT_DIR") == "" {
		if _, err := os.Stat("/apex/com.android.conscrypt/cacerts"); err == nil {
			_ = os.Setenv("SSL_CERT_DIR", "/apex/com.android.conscrypt/cacerts:/system/etc/security/cacerts")
		}
	}
}

//export RcloneInitialize
func RcloneInitialize() {
	oauthutil.OpenURL = func(u string) error {
		openURLMu.RLock()
		fn := openURLFn
		openURLMu.RUnlock()
		if fn != nil {
			cu := C.CString(u)
			rc := C.call_open_url(fn, cu)
			C.free(unsafe.Pointer(cu))
			if rc == 0 {
				return nil
			}
			return errors.New("host open_url handler failed")
		}
		return defaultOpenURL(u)
	}
	librclone.Initialize()
}

//export RcloneSetOpenURL
func RcloneSetOpenURL(fn C.RcloneOpenURLFn) {
	openURLMu.Lock()
	openURLFn = fn
	openURLMu.Unlock()
}

//export RcloneFinalize
func RcloneFinalize() {
	librclone.Finalize()
}

//export RcloneRPC
func RcloneRPC(method *C.char, input *C.char) (result C.struct_RcloneRPCResult) {
	output, status := librclone.RPC(C.GoString(method), C.GoString(input))
	result.Output = C.CString(output)
	result.Status = C.int(status)
	return result
}

//export RcloneFreeString
func RcloneFreeString(str *C.char) {
	C.free(unsafe.Pointer(str))
}

func main() {}
