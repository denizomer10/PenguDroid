# Durum notu (devir teslim)

Bu dosya, kaldığımız yerin dürüst özeti. Başka bir makinede devam edecek kişi
için yazıldı. Ayrıntılı teknik gerekçeler `docs/LIMITATIONS.md` §2.1–2.4.

## Bugün çalışan şeyler

| Ne | Komut | Kanıt |
|---|---|---|
| ART açılıyor, canlı `JavaVM` | `pengudroid art-test --sysroot <tree>` | `JNI_CreateJavaVM returned JNI_OK`, `GetVersion = 0x00010006` |
| Gerçek APK DEX'i yükleniyor | `art-test ... --classpath app.apk --find-class <C>` | sınıf *system class loader*'dan çözülüyor |
| Framework native'leri (150 grup) | `art-test ... --register-natives libandroid_runtime.so` | `registered 150 of 150` |
| ActivityThread.main çağrılıyor | `art-test ... --main android.app.ActivityThread` | "invoking ... main" |
| Bundle açma | `pengudroid extract app.apkm --dest D --abi arm64-v8a` | base + split + OBB yazılıyor |
| Bundle analizi | `pengudroid info/doctor app.apkm` | split'lerdeki 37 kütüphane, ABI listesi, DEX sınıf sayısı |
| x86_64 NativeActivity oyunu | `pengudroid native --lib fixtures/native-game/libmain.so` | 1280×720, ~56 fps (RTX 3080) |

Testler: **153 geçiyor** (`cargo test --workspace`).

## Çalışmayanlar ve tam olarak nerede durduğu

### NewPipe 0.29.1 (`org.schabi.newpipe`) — AÇILMIYOR
```
pengudroid launch NewPipe_v0.29.1.apk --sysroot <tree>
  backend : art (x86_64 libraries but no NativeActivity entry point)
  art: registered 150 of 150 native groups
  art: invoking android.app.ActivityThread.main with 0 argument(s)
  java.lang.NoClassDefFoundError: Class not found using the boot class loader
  art: android.app.ActivityThread.main threw
```
ART tamamen açılıyor, framework native'leri kayıtlı, `ActivityThread.main`
çağrılıyor — sonra native bir `FindClass` boot loader'da sınıfı bulamıyor.
Elimizdeki `bootclasspath` imajdaki `/system/etc/classpaths/bootclasspath.pb`
ile **birebir aynı** (12 jar), yani eksik jar değil. Şüphe: `ActivityThread.main`
çalıştıran thread'in *class loader override*'ı yok (cihazda app_process bunu
`Thread.attach` ile kurar) ve/veya bir sonraki adım `system_server`'a binder
çağrısı. **Sıradaki adım:** ART'ı `-verbose:class`/`-Xlog` ile çalıştırıp eksik
sınıfın adını almak; sonra `Thread.setContextClassLoader` veya
`ActivityThread.systemMain()` yolunu denemek.

### Beach Buggy Racing 2 (`com.vectorunit.cobalt`, .apkm) — AÇILMIYOR
- Bundle sorunsuz açılıyor (36 split, 37 native lib, arm64-v8a + armeabi-v7a,
  9188 DEX sınıfı, SDL3 tabanlı `libmain.so` 17.8 MB).
- `pengudroid launch` prefix'e açıyor, ART'ı başlatıyor, `ActivityThread.main`'e
  geliyor ve orada duruyor (yukarıdaki NewPipe ile aynı sebep).
- Ayrıca **ARM-only**: x86_64 kütüphanesi yok.

### ARM çevirici (Intel NDK translation) — misafir yürütmeye geçemiyor
Kurulum: Google Android 11 (API 30) x86_64 Google-APIs imajı
(`x86_64-30_r16.zip`), `libndk_translation.so` + `/system/lib64/arm64/*` (60 lib)
+ `/system/bin/arm64/{app_process64,linker64}`. `bwrap` ile `/system`, `/apex`,
`/system/lib64` (ARM64 olarak!) bağlanıyor — komut `docs/LIMITATIONS.md` §2.2.

Elde edilenler:
```
LoadNativeBridge(libndk_translation.so) = true
PreInitializeNativeBridge(<dir>, arm64) = true
JNI_CreateJavaVM = JNI_OK            (yalnızca -Xmx128m ile!)
ndk_translation: Initialized NDK translation (aarch64)
```
Yeni ve önemli bulgular:
1. `Itf.initialize` bu sürümde **3 argüman** alıyor:
   `native_bridge_initialize(callbacks, char const*, char const*)`. Tek argümanla
   çağrılırsa `strcmp` çöp pointer'da patlıyor.
2. `Itf.initialize` **hem JIT'i kuruyor hem misafir `/system/bin/arm64/app_process64`
   sürecini başlatıyor** (ayrı thread: `StartGuestExecutableImpl` → `ExecuteGuest`).
   Yani "sadece kütüphane yükle" modu bu API'de yok.
3. Misafir userspace **tutarsız**: `/system/lib64/arm64/` içinde `libart.so` ve
   `libdexfile.so` YOK; ART APEX'i (`/system/apex/com.android.art.debug.apex`
   içindeki `apex_payload.img`) yalnızca x86_64 `lib64/` içeriyor. Yani çevirici
   ARM `app_process64`'ü başlatıyor ama o süreç için ARM ART yok.
4. Ölçümler: gerçek `app_process64` ile süreç `ndk_translation_HandleNoExec`
   içinde ölüyor; elle yazdığım minimal statik ARM64 programıyla (`.toolchain/arm-hello`)
   **çökme yok**, tüm thread'ler futex'te bekliyor (biri `do_sigtimedwait`).
   Çevirici misafir ELF'inde tam section-header tablosu + `.shstrtab` istiyor
   (`e_shentsize: 0x0 (expected 0x40)`, sonra `invalid e_shstrndx`).
5. Düzeltilen gerçek hata: ART `FaultManager::Init` çeviricinin SIGSEGV/SIGBUS/
   SIGILL/SIGFPE handler'larını eziyordu; VM açıldıktan sonra
   `NativeBridgeGetSignalHandler` ile geri kuruluyor.

### ARM çevirici için sıradaki adım
Bir *misafir Android userspace* gerekiyor: ARM bionic init + linker namespace +
**ARM ART** (`libart.so`/`libdexfile.so` arm64) + sonra yine `system_server`.
Bu, bir emülatörün ikinci yarısını yazmak demek. `/dev/kvm` bu makinede var;
tek gerçekçi "oyun açılsın" yolu VM/container, ki bu projenin önermesinin dışında
(old weather karar: kullanıcı henüz VM'i seçmedi).

## Yerel araçlar (git'e girmez, `.toolchain/` ignore'da)
- `.toolchain/armrun.sh` — çevirici için gereken bwrap mount namespace'i.
- `.toolchain/arm-hello` — minimal ARM64 statik ikili (section header'lı).
- `.toolchain/libarmtest.so` — `arm_answer()` → 42 döndüren elle üretilmiş ARM64 `.so`.
- `.toolchain/ndk/x86_64/system.img` — Android 11 x86_64 imajı (sparse değil; ext4
  offset `0x300000`; `EXT4_OFFSET=3145728 python3 .toolchain/ext4.py ...`).
- `.toolchain/ndk-arm/` — çıkarılmış çevirici + ARM runtime.
- `.toolchain/sysroot/` — LineageOS 20 (Android 13) x86_64 sistem ağacı.

## Bu oturumun son commit'leri
`0a82e39` ARM bridge sinyal handler'ları + bulgular · `bfb3675` bundle desteği ·
`02e6c1a` çevirici çalıştırma dokümanı · `cd41f2e` ART JNI_OK + sinyal ABI +
launcher.

## Yeni makinede ilk adımlar
1. `cargo build --workspace` (sistemde Rust yoksa `.toolchain` yeniden kurulmalı).
2. `cargo test --workspace` → 153 test.
3. NewPipe için: `art-test --sysroot <tree> --classpath NewPipe.apk --main android.app.ActivityThread`
   ve eksik sınıfı bulmak için ART log seviyesini yükselt.
4. Çevirici için: `docs/LIMITATIONS.md` §2.2 komutu, sonra misafir ARM ART sorunu.
