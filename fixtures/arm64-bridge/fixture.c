/*
 * Guest-side artifact for the native-bridge boundary.
 *
 * This is ordinary Android ARM64 code: it is what a translated APK library
 * looks like to the bridge. It is built against the API-37 AArch64 sysroot and
 * is never executed on the host directly — the bridge translates it.
 *
 * `pengudroid_fixture_answer` deliberately needs nothing but its own
 * arithmetic, so a successful call through `NativeBridgeItf.getTrampoline`
 * proves the translation path itself rather than a libc round trip.
 * `pengudroid_fixture_answer_with_log` additionally calls the API-37 liblog
 * stub, which proves the guest can reach a platform library at all.
 */

#include <android/log.h>

__attribute__((visibility("default"))) int pengudroid_fixture_answer(void) {
  return 42;
}

__attribute__((visibility("default"))) int pengudroid_fixture_answer_with_log(void) {
  __android_log_print(ANDROID_LOG_INFO, "PenguDroidBridge", "guest reached liblog");
  return 42;
}
