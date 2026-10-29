/* Cameras on macOS through AVFoundation, with camera.cc's C interface:
 * yapfast_camera_list, yapfast_camera_open and yapfast_camera_close, plus
 * yapfast_camera_access for the Camera permission. A session films in NV12,
 * hands each frame over on a serial dispatch queue of its own, and libyuv
 * converts it into I420 planes this camera reuses for every frame, scaled
 * down as camera.cc scales a camera that can't film small enough. */
#import <AVFoundation/AVFoundation.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>

#include <math.h>
#include <stdint.h>
#include <stdlib.h>

#include "libyuv/convert.h"
#include "libyuv/scale.h"
#include "picture.h"

#if !__has_feature(objc_arc)
#error "camera_mac.m needs -fobjc-arc"
#endif

/* The planes stay valid until the callback returns. */
typedef void (*yapfast_camera_frame)(void *user, const struct yapfast_picture *picture);
typedef void (*yapfast_camera_each)(void *user, const char *name, const char *id);
typedef void (*yapfast_camera_answered)(void *user, int allowed);

/* An I420 picture of one size, allocated once. */
struct planes {
  int width;
  int height;
  uint8_t *data;
};

static void planes_resize(struct planes *planes, int width, int height) {
  if (planes->data && width == planes->width && height == planes->height) return;
  free(planes->data);
  size_t chroma = (size_t)((width + 1) / 2) * ((height + 1) / 2);
  planes->data = calloc((size_t)width * height + 2 * chroma, 1);
  planes->width = planes->data ? width : 0;
  planes->height = planes->data ? height : 0;
}

static int chroma_stride(const struct planes *planes) { return (planes->width + 1) / 2; }
static uint8_t *plane_y(const struct planes *planes) { return planes->data; }
static uint8_t *plane_u(const struct planes *planes) {
  return planes->data + (size_t)planes->width * planes->height;
}
static uint8_t *plane_v(const struct planes *planes) {
  return plane_u(planes) + (size_t)chroma_stride(planes) * ((planes->height + 1) / 2);
}

static struct yapfast_picture planes_picture(const struct planes *planes) {
  return (struct yapfast_picture){planes->width, planes->height, plane_y(planes),
                                  plane_u(planes),  plane_v(planes),   planes->width,
                                  chroma_stride(planes), chroma_stride(planes)};
}

/* Converts the frames on the session's queue and hands them to the sink. */
@interface YapfastCameraSink : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate> {
 @public
  int maxWidth;
  int maxHeight;
  yapfast_camera_frame frame;
  void *user;
  /* Set on the queue by yapfast_camera_close: no frame goes out after it. */
  BOOL closed;
  /* Seconds between frames sent on, and when the next is due, as frames'
   * presentation times go: a format that can't film at the rate asked,
   * such as a fixed 60 a second, films faster, and the extra frames are
   * dropped here as camera.cc's capture module drops them. A fixed 60 gives
   * a steady 30, and a fixed 50 a steady 25. */
  double interval;
  double due;
  struct planes converted;
  struct planes scaled;
}
@end

@implementation YapfastCameraSink

- (void)dealloc {
  free(converted.data);
  free(scaled.data);
}

- (void)captureOutput:(AVCaptureOutput *)output
    didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer
           fromConnection:(AVCaptureConnection *)connection {
  if (closed) return;
  CMTime time = CMSampleBufferGetPresentationTimeStamp(sampleBuffer);
  if (CMTIME_IS_NUMERIC(time)) {
    /* A quarter of a frame early still counts, so jitter doesn't drop
     * frames of a camera filming at the rate asked. The next is due a frame
     * after this one, or after when this one was due if it came early, so
     * that a camera filming twice as fast has every other frame half a frame
     * early, well clear of the quarter. A time further back than the frames
     * sent can be means the clock started over. */
    double at = CMTimeGetSeconds(time), early = interval / 4;
    if (due - at > 2 * interval) due = at;
    if (at + early < due) return;
    due = fmax(due, at) + interval;
  }
  CVImageBufferRef image = CMSampleBufferGetImageBuffer(sampleBuffer);
  if (!image) return;
  if (CVPixelBufferLockBaseAddress(image, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) return;
  int width = (int)CVPixelBufferGetWidth(image);
  int height = (int)CVPixelBufferGetHeight(image);
  int failed = -1;
  if (width > 0 && height > 0) {
    planes_resize(&converted, width, height);
    if (converted.data) {
      uint8_t *y = plane_y(&converted), *u = plane_u(&converted), *v = plane_v(&converted);
      int stride = chroma_stride(&converted);
      /* videoSettings asks for NV12, which AVFoundation converts every
       * frame into. */
      OSType format = CVPixelBufferGetPixelFormatType(image);
      if (format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ||
          format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange) {
        failed = NV12ToI420(CVPixelBufferGetBaseAddressOfPlane(image, 0),
                            (int)CVPixelBufferGetBytesPerRowOfPlane(image, 0),
                            CVPixelBufferGetBaseAddressOfPlane(image, 1),
                            (int)CVPixelBufferGetBytesPerRowOfPlane(image, 1), y, width, u, stride,
                            v, stride, width, height);
      }
    }
  }
  CVPixelBufferUnlockBaseAddress(image, kCVPixelBufferLock_ReadOnly);
  if (failed) return;
  /* A device that can't capture as small as asked is scaled down to fit,
   * keeping its shape, at even sizes as encoders want. */
  double scale = fmin(1.0, fmin((double)maxWidth / width, (double)maxHeight / height));
  int fit_width = (int)(width * scale) & ~1;
  int fit_height = (int)(height * scale) & ~1;
  if (fit_width < 2) fit_width = 2;
  if (fit_height < 2) fit_height = 2;
  struct yapfast_picture picture;
  if (fit_width == width && fit_height == height) {
    picture = planes_picture(&converted);
  } else {
    planes_resize(&scaled, fit_width, fit_height);
    if (!scaled.data) return;
    I420Scale(plane_y(&converted), converted.width, plane_u(&converted),
              chroma_stride(&converted), plane_v(&converted), chroma_stride(&converted), width,
              height, plane_y(&scaled), scaled.width, plane_u(&scaled), chroma_stride(&scaled),
              plane_v(&scaled), chroma_stride(&scaled), fit_width, fit_height, kFilterBox);
    picture = planes_picture(&scaled);
  }
  frame(user, &picture);
}

@end

/* A camera filming: its session, and the queue its frames arrive on. */
@interface YapfastCamera : NSObject {
 @public
  AVCaptureSession *session;
  AVCaptureVideoDataOutput *output;
  YapfastCameraSink *sink;
  dispatch_queue_t queue;
}
@end

@implementation YapfastCamera
@end

/* The cameras AVFoundation lists: built in, plugged in, and iPhones nearby
 * (Continuity Camera). */
static NSArray<AVCaptureDevice *> *cameras(void) {
  NSArray<AVCaptureDeviceType> *types = @[
    AVCaptureDeviceTypeBuiltInWideAngleCamera, AVCaptureDeviceTypeExternal,
    AVCaptureDeviceTypeContinuityCamera
  ];
  return [AVCaptureDeviceDiscoverySession
             discoverySessionWithDeviceTypes:types
                                   mediaType:AVMediaTypeVideo
                                    position:AVCaptureDevicePositionUnspecified]
      .devices;
}

/* The range of `format` that films at `fps`, give or take half a frame, so
 * that 29.97 counts as 30; nil when none does. */
static AVFrameRateRange *films_at(AVCaptureDeviceFormat *format, int fps) {
  for (AVFrameRateRange *range in format.videoSupportedFrameRateRanges) {
    if (range.minFrameRate <= fps + 0.5 && range.maxFrameRate >= fps - 0.5) return range;
  }
  return nil;
}

/* 1/`fps` of a second, or the nearest frame duration `range` allows:
 * AVFoundation throws at one outside it, such as 1/30 for a capture device
 * filming at most 29.97 frames a second. */
static CMTime frame_duration(AVFrameRateRange *range, int fps) {
  CMTime duration = CMTimeMake(1, fps);
  if (CMTimeCompare(duration, range.minFrameDuration) < 0) return range.minFrameDuration;
  if (CMTimeCompare(duration, range.maxFrameDuration) > 0) return range.maxFrameDuration;
  return duration;
}

/* The format closest to `width` by `height` at `fps`, as libwebrtc's
 * GetBestMatchedCapability chooses for camera.cc: the smallest at least that
 * large, or else the largest; then one that films at `fps`, then one already
 * in NV12, so nothing converts before libyuv. */
static AVCaptureDeviceFormat *best_format(AVCaptureDevice *device, int width, int height,
                                          int fps) {
  AVCaptureDeviceFormat *best = nil;
  int64_t best_score[4] = {0};
  for (AVCaptureDeviceFormat *format in device.formats) {
    if (![format.mediaType isEqualToString:AVMediaTypeVideo]) continue;
    CMVideoDimensions size = CMVideoFormatDescriptionGetDimensions(format.formatDescription);
    if (size.width <= 0 || size.height <= 0) continue;
    int64_t area = (int64_t)size.width * size.height;
    BOOL covers = size.width >= width && size.height >= height;
    FourCharCode subtype = CMFormatDescriptionGetMediaSubType(format.formatDescription);
    BOOL nv12 = subtype == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ||
                subtype == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
    /* Higher is better, compared in order. */
    int64_t score[4] = {covers, covers ? -area : area, films_at(format, fps) != nil, nv12};
    BOOL better = best == nil;
    for (int index = 0; !better && index < 4; index++) {
      if (score[index] != best_score[index]) {
        better = score[index] > best_score[index];
        break;
      }
    }
    if (better) {
      best = format;
      for (int index = 0; index < 4; index++) best_score[index] = score[index];
    }
  }
  return best;
}

/* 1 when Yapfast may film, and 0 when the person (or a policy) said no.
 * When nobody has asked them yet, it asks: off the main thread it waits for
 * the answer and returns it, and on the main thread, which mustn't wait, it
 * returns -1 and later calls `answered` with `user` and the answer, once,
 * from another thread. It calls `answered` only then. */
int yapfast_camera_access(yapfast_camera_answered answered, void *user) {
  @autoreleasepool {
    switch ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo]) {
      case AVAuthorizationStatusAuthorized:
        return 1;
      case AVAuthorizationStatusDenied:
      case AVAuthorizationStatusRestricted:
        return 0;
      case AVAuthorizationStatusNotDetermined:
      default:
        break;
    }
    if ([NSThread isMainThread]) {
      [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo
                               completionHandler:^(BOOL granted) {
                                 answered(user, granted ? 1 : 0);
                               }];
      return -1;
    }
    __block BOOL allowed = NO;
    dispatch_semaphore_t answered = dispatch_semaphore_create(0);
    [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo
                             completionHandler:^(BOOL granted) {
                               allowed = granted;
                               dispatch_semaphore_signal(answered);
                             }];
    dispatch_semaphore_wait(answered, DISPATCH_TIME_FOREVER);
    return allowed ? 1 : 0;
  }
}

/* Calls `each` with the name and the id that opens it of each camera on this
 * system, from one look at the devices. */
void yapfast_camera_list(yapfast_camera_each each, void *user) {
  @autoreleasepool {
    for (AVCaptureDevice *device in cameras()) {
      const char *name = device.localizedName.UTF8String;
      const char *id = device.uniqueID.UTF8String;
      if (name && id) each(user, name, id);
    }
  }
}

/* Opens the camera with `id` and calls `frame` with each picture, at most
 * `width` by `height` and `fps` a second, on a serial queue of the camera's
 * own, until yapfast_camera_close. */
struct yapfast_camera *yapfast_camera_open(const char *id, int width, int height, int fps,
                                           yapfast_camera_frame frame, void *user) {
  @autoreleasepool {
    if (!id || width <= 0 || height <= 0 || fps <= 0) return NULL;
    AVCaptureDevice *device = [AVCaptureDevice deviceWithUniqueID:@(id)];
    if (!device) return NULL;
    NSError *error = nil;
    AVCaptureDeviceInput *input = [AVCaptureDeviceInput deviceInputWithDevice:device
                                                                         error:&error];
    if (!input) {
      NSLog(@"yapfast: camera %@: %@", device.localizedName, error);
      return NULL;
    }

    YapfastCamera *camera = [YapfastCamera new];
    camera->sink = [YapfastCameraSink new];
    camera->sink->maxWidth = width;
    camera->sink->maxHeight = height;
    camera->sink->interval = 1.0 / fps;
    camera->sink->due = -INFINITY;
    camera->sink->frame = frame;
    camera->sink->user = user;
    camera->queue = dispatch_queue_create("yapfast-camera", DISPATCH_QUEUE_SERIAL);
    camera->session = [AVCaptureSession new];
    camera->output = [AVCaptureVideoDataOutput new];
    camera->output.alwaysDiscardsLateVideoFrames = YES;
    camera->output.videoSettings = @{
      (NSString *)kCVPixelBufferPixelFormatTypeKey :
          @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
    };
    [camera->output setSampleBufferDelegate:camera->sink queue:camera->queue];

    AVCaptureSession *session = camera->session;
    [session beginConfiguration];
    if (![session canAddInput:input] || ![session canAddOutput:camera->output]) {
      [session commitConfiguration];
      return NULL;
    }
    [session addInput:input];
    [session addOutput:camera->output];
    /* The format holds while the device stays locked through startRunning;
     * a session setting its own preset would otherwise replace it. */
    BOOL locked = [device lockForConfiguration:&error];
    AVCaptureDeviceFormat *format = locked ? best_format(device, width, height, fps) : nil;
    if (format) {
      /* A device that still refuses films in its own format, or at that
       * format's own rate, rather than ending the app with an exception. */
      @try {
        device.activeFormat = format;
        AVFrameRateRange *range = films_at(format, fps);
        if (range) device.activeVideoMinFrameDuration = frame_duration(range, fps);
      } @catch (NSException *exception) {
        NSLog(@"yapfast: camera %@ can't film %@ at %d a second: %@", device.localizedName,
              format, fps, exception.reason);
      }
    }
    [session commitConfiguration];
    [session startRunning];
    if (locked) [device unlockForConfiguration];
    if (!session.running) {
      [camera->output setSampleBufferDelegate:nil queue:NULL];
      dispatch_sync(camera->queue, ^{
        camera->sink->closed = YES;
      });
      return NULL;
    }
    return (struct yapfast_camera *)CFBridgingRetain(camera);
  }
}

/* Stops capturing: no frame arrives once this returns. */
void yapfast_camera_close(struct yapfast_camera *raw) {
  if (!raw) return;
  @autoreleasepool {
    YapfastCamera *camera = CFBridgingRelease(raw);
    [camera->session stopRunning];
    [camera->output setSampleBufferDelegate:nil queue:NULL];
    /* Frames already queued run first, and then none goes out. */
    YapfastCameraSink *sink = camera->sink;
    dispatch_sync(camera->queue, ^{
      sink->closed = YES;
    });
  }
}
