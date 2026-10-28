// Go Live's screen capture on macOS: the Screen Recording permission, and
// the windows a process owns, which the share menu leaves out. libwebrtc
// captures; these only ask macOS about it.

#include <CoreFoundation/CoreFoundation.h>
#include <CoreGraphics/CoreGraphics.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/types.h>

// Whether this app may record the screen. macOS answers for the app
// responsible for the process: the .app, or the terminal that started an
// unbundled Yapfast.
int yapfast_screen_recording_allowed(void) {
    return CGPreflightScreenCaptureAccess() ? 1 : 0;
}

// Asks for the permission. macOS shows its dialog only the first time an
// app asks; after that it answers at once without one.
void yapfast_ask_for_screen_recording(void) {
    (void)CGRequestScreenCaptureAccess();
}

// Writes the ids of up to `capacity` windows `pid` owns to `ids`, and
// returns how many it owns. Ids and owners are listed without the
// permission; only titles need it.
size_t yapfast_windows_of(pid_t pid, uint32_t *ids, size_t capacity) {
    CFArrayRef windows =
        CGWindowListCopyWindowInfo(kCGWindowListOptionAll, kCGNullWindowID);
    if (!windows) {
        return 0;
    }
    size_t count = 0;
    CFIndex listed = CFArrayGetCount(windows);
    for (CFIndex at = 0; at < listed; at++) {
        CFDictionaryRef window = CFArrayGetValueAtIndex(windows, at);
        CFNumberRef owner = CFDictionaryGetValue(window, kCGWindowOwnerPID);
        CFNumberRef number = CFDictionaryGetValue(window, kCGWindowNumber);
        int owner_pid = 0;
        uint32_t id = 0;
        if (!owner || !number ||
            !CFNumberGetValue(owner, kCFNumberIntType, &owner_pid) ||
            owner_pid != pid ||
            !CFNumberGetValue(number, kCFNumberSInt32Type, &id)) {
            continue;
        }
        if (count < capacity) {
            ids[count] = id;
        }
        count++;
    }
    CFRelease(windows);
    return count;
}
