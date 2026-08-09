#include <CoreAudio/AudioHardware.h>
#include <stdint.h>

typedef struct {
    AudioDeviceID device_id;
    Float32 volume;
    UInt32 muted;
    UInt32 has_volume;
    UInt32 has_mute;
} CastAudioOutputState;

static AudioObjectPropertyAddress default_output_address(void) {
    return (AudioObjectPropertyAddress) {
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain,
    };
}

static AudioObjectPropertyAddress volume_address(void) {
    return (AudioObjectPropertyAddress) {
        kAudioDevicePropertyVolumeScalar,
        kAudioDevicePropertyScopeOutput,
        kAudioObjectPropertyElementMain,
    };
}

static AudioObjectPropertyAddress mute_address(void) {
    return (AudioObjectPropertyAddress) {
        kAudioDevicePropertyMute,
        kAudioDevicePropertyScopeOutput,
        kAudioObjectPropertyElementMain,
    };
}

OSStatus cast_audio_output_snapshot(CastAudioOutputState *state) {
    if (state == NULL) {
        return kAudioHardwareIllegalOperationError;
    }

    AudioDeviceID device = kAudioObjectUnknown;
    UInt32 size = sizeof(device);
    AudioObjectPropertyAddress output = default_output_address();
    OSStatus status = AudioObjectGetPropertyData(
        kAudioObjectSystemObject, &output, 0, NULL, &size, &device);
    if (status != noErr || device == kAudioObjectUnknown) {
        return status != noErr ? status : kAudioHardwareBadDeviceError;
    }

    state->device_id = device;
    state->volume = 0.0f;
    state->muted = 0;
    state->has_volume = 0;
    state->has_mute = 0;

    AudioObjectPropertyAddress volume = volume_address();
    size = sizeof(state->volume);
    Boolean settable = false;
    if (AudioObjectHasProperty(device, &volume)
        && AudioObjectIsPropertySettable(device, &volume, &settable) == noErr
        && settable
        && AudioObjectGetPropertyData(
            device, &volume, 0, NULL, &size, &state->volume) == noErr) {
        state->has_volume = 1;
    }

    AudioObjectPropertyAddress mute = mute_address();
    size = sizeof(state->muted);
    settable = false;
    if (AudioObjectHasProperty(device, &mute)
        && AudioObjectIsPropertySettable(device, &mute, &settable) == noErr
        && settable
        && AudioObjectGetPropertyData(
            device, &mute, 0, NULL, &size, &state->muted) == noErr) {
        state->has_mute = 1;
    }

    return noErr;
}

OSStatus cast_audio_output_set_volume(AudioDeviceID device, Float32 volume) {
    AudioObjectPropertyAddress address = volume_address();
    UInt32 size = sizeof(volume);
    return AudioObjectSetPropertyData(device, &address, 0, NULL, size, &volume);
}

OSStatus cast_audio_output_set_muted(AudioDeviceID device, UInt32 muted) {
    AudioObjectPropertyAddress address = mute_address();
    UInt32 size = sizeof(muted);
    return AudioObjectSetPropertyData(device, &address, 0, NULL, size, &muted);
}
