// Go Live's sound on macOS: a Core Audio process tap of everything the
// machine plays but Yapfast's own processes, and the private aggregate
// device that records it. Only the tap's description is Objective-C; the
// rest, audio_io/coreaudio/tap.rs does through Core Audio's C functions.
// Everything but yapfast_tap_supported needs macOS 14.4, which only it
// checks: Rust calls the others once it said yes.

#import <CoreAudio/AudioHardware.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#import <Foundation/Foundation.h>

#define TAPS API_AVAILABLE(macos(14.4))

static NSArray<NSNumber *> *numbers(const AudioObjectID *objects, size_t count) {
  NSMutableArray<NSNumber *> *array = [NSMutableArray arrayWithCapacity:count];
  for (size_t at = 0; at < count; at++) {
    [array addObject:@(objects[at])];
  }
  return array;
}

// Whether this macOS taps processes as Yapfast needs: from 14.4 on, where
// the System Audio Recording permission covers taps.
int yapfast_tap_supported(void) {
  if (@available(macOS 14.4, *)) {
    return 1;
  }
  return 0;
}

// Taps everything but the `excluded` process objects, mixed down to stereo
// and still heard on the speakers, and makes a private aggregate device of
// the tap alone, which starts it. The caller closes the tap whether or not
// the device was made.
TAPS OSStatus yapfast_tap_open(const AudioObjectID *excluded, size_t count,
                               AudioObjectID *tap, AudioObjectID *device) {
  *tap = kAudioObjectUnknown;
  *device = kAudioObjectUnknown;
  @autoreleasepool {
    CATapDescription *description = [[CATapDescription alloc]
        initStereoGlobalTapButExcludeProcesses:numbers(excluded, count)];
    description.name = @"Yapfast";
    description.privateTap = YES;
    description.muteBehavior = CATapUnmuted;
    OSStatus status = AudioHardwareCreateProcessTap(description, tap);
    if (status != noErr) {
      *tap = kAudioObjectUnknown;
      return status;
    }
    NSDictionary *aggregate = @{
      @kAudioAggregateDeviceNameKey : @"Yapfast",
      @kAudioAggregateDeviceUIDKey : [[NSUUID UUID] UUIDString],
      @kAudioAggregateDeviceIsPrivateKey : @YES,
      @kAudioAggregateDeviceTapAutoStartKey : @YES,
      @kAudioAggregateDeviceTapListKey : @[ @{
        @kAudioSubTapUIDKey : description.UUID.UUIDString,
        @kAudioSubTapDriftCompensationKey : @YES,
      } ],
    };
    status = AudioHardwareCreateAggregateDevice(
        (__bridge CFDictionaryRef)aggregate, device);
    if (status != noErr) {
      *device = kAudioObjectUnknown;
    }
    return status;
  }
}

// Leaves the `excluded` process objects out of a running tap from now on.
TAPS OSStatus yapfast_tap_exclude(AudioObjectID tap,
                                  const AudioObjectID *excluded,
                                  size_t count) {
  @autoreleasepool {
    AudioObjectPropertyAddress address = {kAudioTapPropertyDescription,
                                          kAudioObjectPropertyScopeGlobal,
                                          kAudioObjectPropertyElementMain};
    CFTypeRef held = NULL;
    UInt32 size = sizeof held;
    OSStatus status =
        AudioObjectGetPropertyData(tap, &address, 0, NULL, &size, &held);
    if (status != noErr || held == NULL) {
      return status != noErr ? status : kAudioHardwareUnspecifiedError;
    }
    // Core Audio hands over a reference the caller releases.
    CATapDescription *description = (__bridge_transfer CATapDescription *)held;
    description.processes = numbers(excluded, count);
    CFTypeRef changed = (__bridge CFTypeRef)description;
    return AudioObjectSetPropertyData(tap, &address, 0, NULL, sizeof changed,
                                      &changed);
  }
}

// Destroys the aggregate device, then the tap; either may be unknown.
TAPS void yapfast_tap_close(AudioObjectID tap, AudioObjectID device) {
  if (device != kAudioObjectUnknown) {
    AudioHardwareDestroyAggregateDevice(device);
  }
  if (tap != kAudioObjectUnknown) {
    AudioHardwareDestroyProcessTap(tap);
  }
}
