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
      switch (CVPixelBufferGetPixelFormatType(image)) {
        case kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange:
        case kCVPixelFormatType_420YpCbCr8BiPlanarFullRange:
          failed = NV12ToI420(CVPixelBufferGetBaseAddressOfPlane(image, 0),
                              (int)CVPixelBufferGetBytesPerRowOfPlane(image, 0),
                              CVPixelBufferGetBaseAddressOfPlane(image, 1),
                              (int)CVPixelBufferGetBytesPerRowOfPlane(image, 1), y, width, u,
                              stride, v, stride, width, height);
          break;
        case kCVPixelFormatType_422YpCbCr8:
          failed = UYVYToI420(CVPixelBufferGetBaseAddress(image),
                              (int)CVPixelBufferGetBytesPerRow(image), y, width, u, stride, v,
                              stride, width, height);
          break;
        case kCVPixelFormatType_422YpCbCr8_yuvs:
          failed = YUY2ToI420(CVPixelBufferGetBaseAddress(image),
                              (int)CVPixelBufferGetBytesPerRow(image), y, width, u, stride, v,
                              stride, width, height);
          break;
        case kCVPixelFormatType_32BGRA:
          /* libyuv's ARGB is BGRA in memory. */
          failed = ARGBToI420(CVPixelBufferGetBaseAddress(image),
                              (int)CVPixelBufferGetBytesPerRow(image), y, width, u, stride, v,
                              stride, width, height);
          break;
        default:
          break;
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

/* Whether `format` can film at `fps`. */
static BOOL films_at(AVCaptureDeviceFormat *format, int fps) {
  for (AVFrameRateRange *range in format.videoSupportedFrameRateRanges) {
    if (range.minFrameRate <= fps + 0.5 && range.maxFrameRate >= fps - 0.5) return YES;
  }
  return NO;
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
    int64_t score[4] = {covers, covers ? -area : area, films_at(format, fps), nv12};
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

/* 1 when Yapfast may film, 0 when the person (or a policy) said no, and -1
 * when nobody has been asked yet and this is the main thread, which mustn't
 * wait for the answer: the question is asked, and the next open knows. Off
 * the main thread it asks and waits for the answer. */
int yapfast_camera_access(void) {
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
                                 (void)granted;
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
    if (locked) {
      AVCaptureDeviceFormat *format = best_format(device, width, height, fps);
      if (format) {
        device.activeFormat = format;
        if (films_at(format, fps)) {
          device.activeVideoMinFrameDuration = CMTimeMake(1, fps);
        }
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
