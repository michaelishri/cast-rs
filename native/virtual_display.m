#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#import <Foundation/Foundation.h>
#import <dispatch/dispatch.h>
#import <objc/runtime.h>

#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

// CGVirtualDisplay is private API. Keep the declarations local, resolve every
// private class by name, and expose only a small C boundary to Rust. This lets
// the ordinary Cast commands continue to start if Apple removes the classes.

@protocol CastVirtualDisplayDescriptorApi <NSObject>
@property(retain, nonatomic) NSString *name;
@property(nonatomic) unsigned int vendorID;
@property(nonatomic) unsigned int productID;
@property(nonatomic) unsigned int serialNum;
@property(nonatomic) unsigned int serialNumber;
@property(nonatomic) unsigned int maxPixelsWide;
@property(nonatomic) unsigned int maxPixelsHigh;
@property(nonatomic) CGSize sizeInMillimeters;
- (void)setDispatchQueue:(dispatch_queue_t)queue;
@end

@protocol CastVirtualDisplayModeApi <NSObject>
- (instancetype)initWithWidth:(NSUInteger)width
                       height:(NSUInteger)height
                  refreshRate:(CGFloat)refreshRate;
@end

@protocol CastVirtualDisplaySettingsApi <NSObject>
@property(retain, nonatomic) NSArray *modes;
@property(nonatomic) unsigned int hiDPI;
@end

@protocol CastVirtualDisplayApi <NSObject>
@property(readonly, nonatomic) CGDirectDisplayID displayID;
- (instancetype)initWithDescriptor:(id)descriptor;
- (BOOL)applySettings:(id)settings;
@end

static __strong id cast_display = nil;
static __strong id cast_descriptor = nil;
static __strong id cast_settings = nil;
static __strong id cast_mode = nil;

static const uint32_t CAST_VENDOR_ID = 0xCA57;
static const uint32_t CAST_PRODUCT_ID = 0x0001;
static const uint32_t CAST_SERIAL_NUMBER = 0x0001;
enum { MAX_DISPLAYS = 32 };

static void set_error(char *buffer, size_t length, const char *message) {
    if (buffer == NULL || length == 0) {
        return;
    }
    snprintf(buffer, length, "%s", message);
}

static void set_descriptor_serial(id<CastVirtualDisplayDescriptorApi> descriptor,
                                  uint32_t serial_number) {
    descriptor.serialNum = serial_number;
    if ([descriptor respondsToSelector:@selector(setSerialNumber:)]) {
        descriptor.serialNumber = serial_number;
    }
}

static bool cast_display_is_already_online(CGDirectDisplayID *existing_id) {
    CGDirectDisplayID displays[MAX_DISPLAYS];
    uint32_t count = 0;
    if (CGGetOnlineDisplayList((uint32_t)MAX_DISPLAYS, displays, &count) !=
        kCGErrorSuccess) {
        return false;
    }
    for (uint32_t index = 0; index < count; index++) {
        CGDirectDisplayID display_id = displays[index];
        if (CGDisplayVendorNumber(display_id) == CAST_VENDOR_ID &&
            CGDisplayModelNumber(display_id) == CAST_PRODUCT_ID &&
            CGDisplaySerialNumber(display_id) == CAST_SERIAL_NUMBER) {
            if (existing_id != NULL) {
                *existing_id = display_id;
            }
            return true;
        }
    }
    return false;
}

static bool online_display_list_contains(CGDirectDisplayID display_id) {
    if (display_id == kCGNullDirectDisplay) {
        return false;
    }
    CGDirectDisplayID displays[MAX_DISPLAYS];
    uint32_t count = 0;
    if (CGGetOnlineDisplayList((uint32_t)MAX_DISPLAYS, displays, &count) !=
        kCGErrorSuccess) {
        return CGDisplayIsOnline(display_id);
    }
    for (uint32_t index = 0; index < count; index++) {
        if (displays[index] == display_id) {
            return true;
        }
    }
    return false;
}

static bool private_api_is_usable(Class descriptor_class, Class mode_class,
                                  Class settings_class, Class display_class) {
    return class_getInstanceMethod(descriptor_class, @selector(init)) != NULL &&
           class_getInstanceMethod(descriptor_class, @selector(setName:)) != NULL &&
           class_getInstanceMethod(descriptor_class, @selector(setVendorID:)) != NULL &&
           class_getInstanceMethod(descriptor_class, @selector(setProductID:)) != NULL &&
           class_getInstanceMethod(descriptor_class, @selector(setSerialNum:)) != NULL &&
           class_getInstanceMethod(descriptor_class, @selector(setMaxPixelsWide:)) != NULL &&
           class_getInstanceMethod(descriptor_class, @selector(setMaxPixelsHigh:)) != NULL &&
           class_getInstanceMethod(descriptor_class, @selector(setSizeInMillimeters:)) != NULL &&
           class_getInstanceMethod(descriptor_class, @selector(setDispatchQueue:)) != NULL &&
           class_getInstanceMethod(
               mode_class, @selector(initWithWidth:height:refreshRate:)) != NULL &&
           class_getInstanceMethod(settings_class, @selector(init)) != NULL &&
           class_getInstanceMethod(settings_class, @selector(setHiDPI:)) != NULL &&
           class_getInstanceMethod(settings_class, @selector(setModes:)) != NULL &&
           class_getInstanceMethod(display_class, @selector(initWithDescriptor:)) != NULL &&
           class_getInstanceMethod(display_class, @selector(applySettings:)) != NULL &&
           class_getInstanceMethod(display_class, @selector(displayID)) != NULL;
}

static int32_t right_edge_before_creation(void) {
    CGDirectDisplayID displays[MAX_DISPLAYS];
    uint32_t count = 0;
    if (CGGetActiveDisplayList((uint32_t)MAX_DISPLAYS, displays, &count) !=
        kCGErrorSuccess) {
        return 0;
    }
    double right_edge = 0.0;
    for (uint32_t index = 0; index < count; index++) {
        right_edge = fmax(right_edge, CGRectGetMaxX(CGDisplayBounds(displays[index])));
    }
    if (right_edge > INT32_MAX) {
        return INT32_MAX;
    }
    return (int32_t)right_edge;
}

static bool wait_until_online(CGDirectDisplayID display_id) {
    for (uint32_t attempt = 0; attempt < 50; attempt++) {
        if (online_display_list_contains(display_id)) {
            return true;
        }
        usleep(100000);
    }
    return false;
}

static void release_cast_display(void) {
    cast_display = nil;
    cast_settings = nil;
    cast_mode = nil;
    cast_descriptor = nil;
}

static CGError configure_as_extension(CGDirectDisplayID display_id,
                                      int32_t right_edge) {
    CGDisplayConfigRef configuration = NULL;
    CGError result = CGBeginDisplayConfiguration(&configuration);
    if (result != kCGErrorSuccess) {
        return result;
    }
    if (configuration == NULL) {
        return kCGErrorFailure;
    }

    CGDirectDisplayID displays[MAX_DISPLAYS];
    uint32_t count = 0;
    if (CGGetOnlineDisplayList((uint32_t)MAX_DISPLAYS, displays, &count) ==
        kCGErrorSuccess) {
        for (uint32_t index = 0; index < count; index++) {
            CGDirectDisplayID candidate = displays[index];
            if (candidate == display_id ||
                CGDisplayMirrorsDisplay(candidate) == display_id) {
                result = CGConfigureDisplayMirrorOfDisplay(
                    configuration, candidate, kCGNullDirectDisplay);
                if (result != kCGErrorSuccess) {
                    CGCancelDisplayConfiguration(configuration);
                    return result;
                }
            }
        }
    }

    result = CGConfigureDisplayOrigin(configuration, display_id, right_edge, 0);
    if (result != kCGErrorSuccess) {
        CGCancelDisplayConfiguration(configuration);
        return result;
    }
    return CGCompleteDisplayConfiguration(configuration, kCGConfigureForSession);
}

uint32_t cast_virtual_display_create(uint32_t width, uint32_t height,
                                     uint32_t frames_per_second,
                                     char *error_buffer,
                                     size_t error_buffer_length) {
    @autoreleasepool {
        if (width < 2 || height < 2 || frames_per_second == 0) {
            set_error(error_buffer, error_buffer_length,
                      "virtual display dimensions and frame rate must be positive");
            return 0;
        }
        if (cast_display != nil) {
            set_error(error_buffer, error_buffer_length,
                      "this helper already owns a virtual display");
            return 0;
        }

        CGDirectDisplayID existing_id = kCGNullDirectDisplay;
        if (cast_display_is_already_online(&existing_id)) {
            char message[160];
            snprintf(message, sizeof(message),
                     "another Cast extended display is already active (display %u)",
                     existing_id);
            set_error(error_buffer, error_buffer_length, message);
            return 0;
        }

        Class descriptor_class = NSClassFromString(@"CGVirtualDisplayDescriptor");
        Class mode_class = NSClassFromString(@"CGVirtualDisplayMode");
        Class settings_class = NSClassFromString(@"CGVirtualDisplaySettings");
        Class display_class = NSClassFromString(@"CGVirtualDisplay");
        if (descriptor_class == Nil || mode_class == Nil ||
            settings_class == Nil || display_class == Nil ||
            !private_api_is_usable(descriptor_class, mode_class, settings_class,
                                   display_class)) {
            set_error(error_buffer, error_buffer_length,
                      "the private CGVirtualDisplay API is unavailable on this macOS build");
            return 0;
        }

        NSApplication *application = [NSApplication sharedApplication];
        [application setActivationPolicy:NSApplicationActivationPolicyProhibited];

        int32_t right_edge = right_edge_before_creation();
        id<CastVirtualDisplayDescriptorApi> descriptor =
            [(id)descriptor_class new];
        if (descriptor == nil) {
            set_error(error_buffer, error_buffer_length,
                      "could not create a CGVirtualDisplay descriptor");
            return 0;
        }
        descriptor.name = @"Cast Extended Display";
        descriptor.vendorID = CAST_VENDOR_ID;
        descriptor.productID = CAST_PRODUCT_ID;
        set_descriptor_serial(descriptor, CAST_SERIAL_NUMBER);
        descriptor.maxPixelsWide = width;
        descriptor.maxPixelsHigh = height;
        double pixel_diagonal = hypot((double)width, (double)height);
        double millimeter_diagonal = 27.0 * 25.4;
        descriptor.sizeInMillimeters = CGSizeMake(
            millimeter_diagonal * (double)width / pixel_diagonal,
            millimeter_diagonal * (double)height / pixel_diagonal);
        [descriptor setDispatchQueue:
            dispatch_get_global_queue(DISPATCH_QUEUE_PRIORITY_HIGH, 0)];

        id<CastVirtualDisplayModeApi> mode =
            [[(id)mode_class alloc] initWithWidth:width
                                           height:height
                                      refreshRate:(CGFloat)frames_per_second];
        id<CastVirtualDisplaySettingsApi> settings = [(id)settings_class new];
        if (mode == nil || settings == nil) {
            set_error(error_buffer, error_buffer_length,
                      "could not create the requested virtual display mode");
            return 0;
        }
        settings.hiDPI = 0;
        settings.modes = @[ mode ];

        id<CastVirtualDisplayApi> display =
            [[(id)display_class alloc] initWithDescriptor:descriptor];
        if (display == nil || ![display applySettings:settings] ||
            display.displayID == kCGNullDirectDisplay) {
            set_error(error_buffer, error_buffer_length,
                      "macOS rejected the requested virtual display");
            return 0;
        }

        cast_descriptor = descriptor;
        cast_mode = mode;
        cast_settings = settings;
        cast_display = display;
        CGDirectDisplayID display_id = display.displayID;
        if (!wait_until_online(display_id)) {
            release_cast_display();
            set_error(error_buffer, error_buffer_length,
                      "the virtual display did not become visible to WindowServer");
            return 0;
        }

        CGError configuration_result =
            configure_as_extension(display_id, right_edge);
        if (configuration_result != kCGErrorSuccess) {
            release_cast_display();
            char message[160];
            snprintf(message, sizeof(message),
                     "could not place the virtual display in extended mode (CoreGraphics error %d)",
                     configuration_result);
            set_error(error_buffer, error_buffer_length, message);
            return 0;
        }
        return display_id;
    }
}

void cast_virtual_display_destroy(void) {
    @autoreleasepool {
        release_cast_display();
    }
}

bool cast_virtual_display_is_online(uint32_t display_id) {
    return online_display_list_contains(display_id);
}
