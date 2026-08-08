#include <AudioToolbox/AudioToolbox.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

typedef struct CastAacEncoder {
    AudioConverterRef converter;
    uint32_t maximum_packet_size;
} CastAacEncoder;

typedef struct CastPcmInput {
    const float *left;
    const float *right;
    uint32_t frames;
    int supplied;
} CastPcmInput;

static OSStatus cast_aac_input_callback(
    AudioConverterRef converter,
    UInt32 *packet_count,
    AudioBufferList *data,
    AudioStreamPacketDescription **packet_description,
    void *user_data
) {
    (void)converter;
    (void)packet_description;
    CastPcmInput *input = (CastPcmInput *)user_data;
    if (input->supplied || *packet_count == 0) {
        *packet_count = 0;
        return noErr;
    }

    UInt32 frames = input->frames;
    if (frames > *packet_count) {
        frames = *packet_count;
    }
    data->mNumberBuffers = 2;
    data->mBuffers[0].mNumberChannels = 1;
    data->mBuffers[0].mDataByteSize = frames * sizeof(float);
    data->mBuffers[0].mData = (void *)input->left;
    data->mBuffers[1].mNumberChannels = 1;
    data->mBuffers[1].mDataByteSize = frames * sizeof(float);
    data->mBuffers[1].mData = (void *)input->right;
    *packet_count = frames;
    input->supplied = 1;
    return noErr;
}

CastAacEncoder *cast_aac_encoder_create(
    uint32_t sample_rate,
    uint32_t channels,
    uint32_t bitrate,
    uint32_t *maximum_packet_size
) {
    if (channels != 2 || maximum_packet_size == NULL) {
        return NULL;
    }

    AudioStreamBasicDescription input = {0};
    input.mSampleRate = sample_rate;
    input.mFormatID = kAudioFormatLinearPCM;
    input.mFormatFlags = kAudioFormatFlagIsFloat |
                         kAudioFormatFlagIsPacked |
                         kAudioFormatFlagIsNonInterleaved;
    input.mBytesPerPacket = sizeof(float);
    input.mFramesPerPacket = 1;
    input.mBytesPerFrame = sizeof(float);
    input.mChannelsPerFrame = channels;
    input.mBitsPerChannel = 8 * sizeof(float);

    AudioStreamBasicDescription output = {0};
    output.mSampleRate = sample_rate;
    output.mFormatID = kAudioFormatMPEG4AAC;
    output.mChannelsPerFrame = channels;
    UInt32 output_size = sizeof(output);
    if (AudioFormatGetProperty(
            kAudioFormatProperty_FormatInfo,
            0,
            NULL,
            &output_size,
            &output
        ) != noErr) {
        return NULL;
    }

    AudioConverterRef converter = NULL;
    if (AudioConverterNew(&input, &output, &converter) != noErr || converter == NULL) {
        return NULL;
    }
    UInt32 requested_bitrate = bitrate;
    if (AudioConverterSetProperty(
            converter,
            kAudioConverterEncodeBitRate,
            sizeof(requested_bitrate),
            &requested_bitrate
        ) != noErr) {
        AudioConverterDispose(converter);
        return NULL;
    }

    UInt32 packet_size = 0;
    UInt32 packet_size_size = sizeof(packet_size);
    if (AudioConverterGetProperty(
            converter,
            kAudioConverterPropertyMaximumOutputPacketSize,
            &packet_size_size,
            &packet_size
        ) != noErr || packet_size == 0) {
        AudioConverterDispose(converter);
        return NULL;
    }

    CastAacEncoder *encoder = calloc(1, sizeof(*encoder));
    if (encoder == NULL) {
        AudioConverterDispose(converter);
        return NULL;
    }
    encoder->converter = converter;
    encoder->maximum_packet_size = packet_size;
    *maximum_packet_size = packet_size;
    return encoder;
}

int cast_aac_encoder_encode(
    CastAacEncoder *encoder,
    const float *left,
    const float *right,
    uint32_t frames,
    uint8_t *output,
    uint32_t output_capacity,
    uint32_t *output_length
) {
    if (encoder == NULL || left == NULL || right == NULL || output == NULL ||
        output_length == NULL || output_capacity < encoder->maximum_packet_size) {
        return -1;
    }

    CastPcmInput input = {left, right, frames, 0};
    AudioBufferList output_buffers = {0};
    output_buffers.mNumberBuffers = 1;
    output_buffers.mBuffers[0].mNumberChannels = 2;
    output_buffers.mBuffers[0].mDataByteSize = output_capacity;
    output_buffers.mBuffers[0].mData = output;
    UInt32 packet_count = 1;
    AudioStreamPacketDescription packet_description = {0};
    OSStatus status = AudioConverterFillComplexBuffer(
        encoder->converter,
        cast_aac_input_callback,
        &input,
        &packet_count,
        &output_buffers,
        &packet_description
    );
    if (status != noErr) {
        return (int)status;
    }
    if (packet_count == 0) {
        *output_length = 0;
        return 0;
    }
    if (packet_description.mStartOffset < 0 ||
        packet_description.mDataByteSize > output_capacity ||
        (uint64_t)packet_description.mStartOffset + packet_description.mDataByteSize >
            output_capacity) {
        return -2;
    }
    if (packet_description.mStartOffset != 0) {
        memmove(
            output,
            output + packet_description.mStartOffset,
            packet_description.mDataByteSize
        );
    }
    *output_length = packet_description.mDataByteSize;
    return 0;
}

void cast_aac_encoder_destroy(CastAacEncoder *encoder) {
    if (encoder == NULL) {
        return;
    }
    if (encoder->converter != NULL) {
        AudioConverterDispose(encoder->converter);
    }
    free(encoder);
}
