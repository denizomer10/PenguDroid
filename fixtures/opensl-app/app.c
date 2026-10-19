/*
 * Minimal OpenSL ES "buffer queue player" smoke test.
 *
 * The call sequence and structures below are the ordinary Android playback
 * path, compiled against the canonical Khronos/AOSP headers. It enqueues
 * silence only, so running it is inaudible.
 */

/*
 * OpenSLES_Platform.h spells deprecation as Clang's
 * `availability(android, ...)`. GCC does not know that attribute, so hide it;
 * the headers themselves stay unmodified.
 */
#ifndef __clang__
#define SL_API_DEPRECATED(level)
#endif

#include <SLES/OpenSLES.h>
#include <SLES/OpenSLES_Android.h>

#include <stdint.h>
#include <stdio.h>
#include <unistd.h>

/* 4096 bytes of S16LE stereo @ 48 kHz is ~21 ms; zeroed = silence. */
static uint8_t g_silence[4096];
static int g_callbacks;

static SLAndroidSimpleBufferQueueItf g_bq;

static void bq_callback(SLAndroidSimpleBufferQueueItf bq, void *context)
{
    (void)context;
    g_callbacks++;
    /* Re-enqueue the same silent buffer so playback keeps advancing. */
    (void)(*bq)->Enqueue(bq, g_silence, (SLuint32)sizeof(g_silence));
}

#define CHECK(expr)                                                           \
    do {                                                                      \
        SLresult r_ = (expr);                                                 \
        if (r_ != SL_RESULT_SUCCESS) {                                        \
            fprintf(stderr, "opensl: %s failed: %u\n", #expr, (unsigned)r_);  \
            return 1;                                                         \
        }                                                                     \
    } while (0)

int main(void)
{
    SLObjectItf engine = NULL;
    SLEngineItf engine_itf = NULL;
    SLObjectItf mix = NULL;
    SLObjectItf player = NULL;
    SLPlayItf play = NULL;

    CHECK(slCreateEngine(&engine, 0, NULL, 0, NULL, NULL));
    CHECK((*engine)->Realize(engine, SL_BOOLEAN_FALSE));
    CHECK((*engine)->GetInterface(engine, SL_IID_ENGINE, &engine_itf));

    /* No interfaces are mandatory at creation time; we fetch them below. */
    CHECK((*engine_itf)->CreateOutputMix(engine_itf, &mix, 0, NULL, NULL));
    CHECK((*mix)->Realize(mix, SL_BOOLEAN_FALSE));

    SLDataLocator_AndroidSimpleBufferQueue loc_bq = {
        SL_DATALOCATOR_ANDROIDSIMPLEBUFFERQUEUE,
        2
    };
    SLDataFormat_PCM format_pcm = {
        SL_DATAFORMAT_PCM,
        2,                          /* stereo */
        SL_SAMPLINGRATE_48,         /* milliHertz: 48000000 */
        SL_PCMSAMPLEFORMAT_FIXED_16,
        SL_PCMSAMPLEFORMAT_FIXED_16,
        SL_SPEAKER_FRONT_LEFT | SL_SPEAKER_FRONT_RIGHT,
        SL_BYTEORDER_LITTLEENDIAN
    };
    SLDataSource source = { &loc_bq, &format_pcm };

    SLDataLocator_OutputMix loc_outmix = { SL_DATALOCATOR_OUTPUTMIX, mix };
    SLDataSink sink = { &loc_outmix, NULL };

    CHECK((*engine_itf)->CreateAudioPlayer(engine_itf, &player, &source, &sink,
                                           0, NULL, NULL));
    CHECK((*player)->Realize(player, SL_BOOLEAN_FALSE));
    CHECK((*player)->GetInterface(player, SL_IID_PLAY, &play));
    CHECK((*player)->GetInterface(player, SL_IID_ANDROIDSIMPLEBUFFERQUEUE,
                                  &g_bq));

    CHECK((*g_bq)->RegisterCallback(g_bq, bq_callback, NULL));
    CHECK((*g_bq)->Enqueue(g_bq, g_silence, (SLuint32)sizeof(g_silence)));

    CHECK((*play)->SetPlayState(play, SL_PLAYSTATE_PLAYING));

    SLuint32 state = SL_PLAYSTATE_STOPPED;
    CHECK((*play)->GetPlayState(play, &state));
    int playing = (state == SL_PLAYSTATE_PLAYING);

    /* Let the sink consume a little audio. */
    usleep(200000);

    SLAndroidSimpleBufferQueueState bq_state;
    CHECK((*g_bq)->GetState(g_bq, &bq_state));

    CHECK((*play)->SetPlayState(play, SL_PLAYSTATE_STOPPED));

    printf("opensl: engine realized, player %s, %d callbacks\n",
           playing ? "PLAYING" : "NOT PLAYING", g_callbacks);
    printf("opensl: queue count=%u index=%u\n",
           (unsigned)bq_state.count, (unsigned)bq_state.index);

    (*player)->Destroy(player);
    (*mix)->Destroy(mix);
    (*engine)->Destroy(engine);

    return playing ? 0 : 1;
}
