# Durum notu (devir teslim)

Bu dosya, kaldığımız yerin dürüst özeti. Başka bir makinede devam edecek kişi
için yazıldı. Ayrıntılı teknik gerekçeler `docs/LIMITATIONS.md` §2.1–2.4.

## Bugün eklenen (bu oturum)

- **`sysroot` eksikti, ART değil.** NewPipe'ın `NoClassDefFoundError`'ı geri
geldi çünkü `.toolchain/sysroot`'ta `apex -> system/apex` bağlantısı yoktu
  (`tools/art/sysroot-relink.py` ile kuruldu) ve imajdaki 25 APEX'in yalnızca
  15'i çıkarılmıştı. Eksik APEX'lerin `etc`/`javalib`/`lib64` alt ağaçları
  çıkarılınca boot classpath **13 → 31 jar** oldu; `android.app.StatsManager` ve
  `android.os.StatsDimensionsValue` (`framework-statsd.jar`, statsd APEX'inin
  *bootclasspath* parçası) çözülür oldu. **Ders:** bir APEX'in `javalib/`ini
  atlamak, `SystemServiceRegistry`'yi ve dolayısıyla `ActivityThread.main`'i
  sessizce `NoClassDefFoundError`'a düşürüyor.
- **binder: hizasız `TransactionData` okuması.** `handle_write_read` struct'ı
  `*const TransactionData` üzerinden okuyordu; komut akışı 4 bayt hizalı olduğu
  için adres `…234` gibi bitiyor ve Rust "misaligned pointer dereference" ile
  **süreci abort ediyordu**. `transaction_at()` + `read_unaligned` ile
düzeltildi, hizasız tamponla test eklendi. Bu düzeltmeden sonra NewPipe
  `ActivityThread.main` → `Looper.loop()` yolunda **45 sn canlı kalıyor**
  (exit 124), yani belgelenen "binder sınırı" durumu geri geldi.
- Boot classpath'i eksiksiz kurmanın yolu (yerel, tek seferlik):
  ```sh
  python3 tools/art/sysroot-relink.py .toolchain/sysroot
  # her APEX için etc/ javalib/ lib64/ alt ağaçlarını çıkar (ext4.py ile)
  ```

## bindApplication — ÇALIŞIYOR (bu tur)

`handleBindApplication` **baştan sona tamamlanıyor** (`apphost: handleBindApplication
returned normally`), hem varsayılan JIT/nterp ile hem `-Xint` ile. Uygulama
bağlanıyor: `GraphicsEnvironment`, `NetworkSecurityConfig`, `LoadedApk`,
`Application` nesnesi, veri dizinleri.

### Grafik duvarı aslında bir `stat64` hatasıydı

Dokümanda "minikin font koleksiyonu boş, native `Typeface` üretilemiyor" diye
kayıtlı duvarın kökü **font değil, shim**di:

- libcore'un `File.isFile()`/`exists()` yolu `stat64`/`lstat64` çağırıyor
  (libopenjdk'nın import listesinde `stat64@LIBC`). Shim yalnızca `stat`,
  `lstat`, `fstatat` interpose ediyordu; `stat64` glibc'ye düşüyor ve **sanal
  kök eşlemesini hiç görmüyordu**. Sonuç: `/system/fonts/*` her sorguda "yok".
- `FontListParser.readFont` `file.isFile()` false olunca fontu atıyor → her
  aile boş → `FontConfig` 0 aile → `SystemFonts.buildSystemFallback` boş →
  `Typeface.setSystemFontMap` boş harita → `Typeface.create(null, 0)` NPE.
- Düzeltme: `stat64`, `lstat64`, `fstat64`, `fstatat64` shim'e eklendi.
  Ölçüm: `--fontmap` **0 → 33 aile**; `FontListParser` 158 aile / 367 font
  okuyor; `Font.Builder.build()` zaten çalışıyordu.

`seed_font_map` (elle font haritası kurma) artık gereksiz ve zararlıydı
(`Typeface.DEFAULT` henüz null olduğu için kendisi NPE veriyordu) — kaldırıldı.

### Sırayla çözülen diğer engeller

1. **`Build$VERSION.<clinit>`** `ArrayIndexOutOfBoundsException: length=0`:
   `getStringList` boş özellikte **sıfır uzunluklu** dizi döndürüyor ve
   `ALL_CODENAMES[0]` patlıyor. `ro.build.version.all_codenames` (ve
   `known_codenames`, `release_or_codename`, `preview_sdk*`, `security_patch`,
   `ro.boot.*`, `ro.soc.*`, ...) shim'in deposuna eklendi (28 → 45).
2. **`mCoreSettings` null**: normalde AMS `updateCoreSettings` ile gönderir;
   bizde boş bir `Bundle` atanıyor. (Bu olmadan nterp *null-check yapmadan*
   alan okuduğu için süreç segfault ediyordu — `-Xint` ile aynı yer temiz bir
   NPE veriyordu. **İpucu: çökme yerini bulmak için `--vm-option -Xint`.**)
3. **`deviceProtectedDataDir`/`credentialProtectedDataDir`**: `setupGraphicsSupport`
   `createDeviceProtectedStorageContext().getCodeCacheDir()` üzerinden bu
   alanları okuyor; null olunca "No data directory found for package".
4. **`IPackageManager.getPackageInfo`** (transaction **kodu 3**):
   `LoadedApk.initializeJavaContextClassLoader` null sonuçta "is package not
   installed?" fırlatıyor.
5. **`IPackageManager.getApplicationInfo`** (transaction **kodu 9**):
   `handleBindApplication` `info.metaData` okuyor, null'da NPE.

4 ve 5 için parcel **elle yazılmıyor**: host, framework'ün kendi
`PackageInfo.writeToParcel`/`ApplicationInfo.writeToParcel`'ını çağırıp
`Parcel.marshall()` ile baytları alıyor ve shim'e aktarıyor
(`__pengudroid_binder_set_package_info` / `..._set_application_info`). Böylece
düzen sürüme özgü tahminlere kalmıyor. Yanıt parcel'ı
`[status=0][presence=1][nesne]` — `readTypedObject` önce varlık int'ini okuyor;
bu 4 bayt eksik olunca okuyucu kayıyor ve 7.6 GB'lık bir tahsis deniyordu.

### Teşhis iyileştirmeleri

- `handleBindApplication` artık **ana thread'de** çağrılıyor; `ActivityThread.main`
  bir yardımcı thread'de koşuyor. Böylece çökmeler VM'i kuran thread'de oluyor.
- `PENGUDROID_TRACE_BINDER=1` artık hedef handle'ın **servis adını** da yazıyor
  (`dispatch target=1 (package) code=9`), yani transaction kodları doğrudan
  okunabiliyor.
- `PENGUDROID_FONTDEBUG=1|2`: font zincirini (config aileleri, fallback,
  `Font.Builder.build`, `XmlPullParser` olay akışı, `File.isFile`) adım adım ölçer.

### Sıradaki adım

Uygulama bağlandı ama **Activity başlamadı**: `system_server`'ın
`scheduleLaunchActivity` çağrısı yok. Sırada:
1. `IApplicationThread.scheduleLaunchActivity` (ActivityClientRecord parcel'ı)
   → `onCreate/onStart/onResume`.
2. Pencere yolu: `SurfaceFlinger`/`SurfaceComposerClient` yerine kendi
   EGL/dmabuf sunumumuz; HWUI EGL üzerinden çiziyor.
3. `IPackageManager`'in geri kalanı (`getPackagesForUid`, `checkPermission`, ...)
   ve `IActivityManager`'in temel metotları.

## Binder sunucu tarafı — durum

Hedef: `system_server`'ın yaptığı gibi uygulamaya *çağrı yapmak*
(`bindApplication`, `scheduleLaunchActivity`).

**Yapılanlar**

- **Uygulama düğümü yakalanıyor.** `attachApplication` parcel'inde
  `BINDER_TYPE_BINDER` (`0x73622a85`) aranıp iki alan saklanıyor: libbinder yerel
  bir binder'ı `obj.binder = getWeakRefs()`, `obj.cookie = BBinder*` olarak
  düzleştirir; gelen transaction'da `target.ptr` = weakref, `cookie` = BBinder.
- **Gelen transaction üretimi** (`incoming_transaction`): `BR_TRANSACTION` +
  `binder_transaction_data`, parcel aynı transfer alanına yazılıyor.
- Ölçülen iki tuzak: (a) transaction `BR_REPLY` ile *aynı* okuma tamponunda
  olmamalı (libbinder `waitForResponse` reply'ı bulunca kalanı atıyor);
  (b) çağıran `read_size = 0` verirse enjekte edilen komut `ENOSPC` yolunda
  düşer — probe bekletiliyor.

**Yeni: uygulamaya çağrı yapılabiliyor (bu turun kazanımı).**

Gerçek sürücü, sıraya girmiş bir transaction'ı **reply'dan önce** teslim eder.
libbinder'ın `waitForResponse`'u `BR_TRANSACTION`'ı `default:` dalında işler ve
*sonra* beklediği `BR_REPLY`'ı okur. Bu yüzden transaction'ı reply'dan önce
koymak, binder thread pool'u olmadan da uygulamaya çağrı indirir:

```
[binder] captured app thread weak=0x… cookie=0x…
[binder] delivering code=0x1 to app node … (read_size=256, before 1 reply)
[binder]   app reply (100 bytes) … "…er invocation to an incorrect interface"
```

`cmd=0x40406301` (**BC_REPLY**) gelmesi uygulamanın bizim transaction'ımızı
*işlediğini* kanıtlar. `INTERFACE_TRANSACTION` (0x5f4e5446) ile
`BC_REPLY data=68` dönüyor ve içinde `IApplicationThread` descriptor'ı var.

Ölçülen ayrıntılar:
- **AIDL kodu 1 geçerli bir metottur** (yanıt `enforceInterface` hatası veriyor).
- Hata mesajı "…invocation to an incorrect **interface**" → parcel'ın başında
  interface token'ı istiyor. `string16("android.app.IApplicationThread")`
  eklendi; şu an hata aynı kaldığı için token biçimi/sırası hâlâ yanlış
  (yanıt parcel'ının 12 baytlık bir ön-eki var, yani exception marshalling'i
  çözülmeden ilerlemek zor).
- Probe bir koşuda tek kod deniyor: uygulama ilk delivery'den sonra yeni
  `read_size > 0` çağrısı yapmıyor (pool yok). Kodları `—PROBE_CODE=1,2,3`
  listesiyle vermek mümkün ama her kod ayrı koşu istiyor.

**Sıradaki en kısa yol:** parcel arkeolojisi yerine Java tarafını kullanmak:
`ActivityThread.currentActivityThread()` → `getApplicationThread()` ile
uygulamanın kendi `ApplicationThread` nesnesini alıp `bindApplication(...)`'ı
**JNI ile doğrudan** çağırmak (parcel, kod numarası ve `enforceInterface`
tamamen devre dışı kalır). Argümanların çoğu `null`, `ApplicationInfo` ise
reflection ile doldurulacak.

**Duvar: binder thread pool.** Uygulama süreci normalde thread pool'unu
*zygote*'tan miras alır; biz zygote'tan fork etmediğimiz için hiçbir thread
`read_size > 0` ile okuma yapmıyor ve `system_server` geri-çağrısı inecek yer
bulamıyor. `ProcessState::self()` + `startThreadPool()` çağrısı
(`PENGUDROID_BINDER_POOL=1`) **`ProcessState::init` içinde SIGSEGV** veriyor:
hata libbinder'ın kendi *kod* segmentine store (`r15` text içinde), yani init'in
beklediği nesne/C++ runtime durumu host tarafından sağlanamıyor. Uygulamanın
*kendi* binder kullanımı çalıştığı için doğru yol, pool'u `ProcessState`'i
oluşturan aynı bağlamdan (bir JNI girişinden) başlatmak.

**Sonrasında gerekenler (küçükten büyüğe)**
1. `BIND_APPLICATION` AIDL kod numarası + `AppBindData` parcel'ı
   (`ApplicationInfo`, provider listesi, `Configuration`, `CompatibilityInfo`).
2. `scheduleLaunchActivity` (ActivityClientRecord) → `onCreate/onStart/onResume`.
3. Pencere yolu: `SurfaceFlinger`/`SurfaceComposerClient` yerine kendi
   EGL/dmabuf sunumumuz; `HWUI` EGL üzerinden çiziyor.
4. Video için `MediaCodec` (codec2/OMX) + `AudioFlinger`/`AudioTrack`.

Not: 3 ve 4, Waydroid'in gerçek Android userspace'i boot ederek çözdüğü
katmanlar. Bu depo "uygulamaları host süreci olarak çalıştır" diyor; o çizgide
kalındığı sürece bu katmanların sırayla yazılması gerekiyor.

## Bugün çalışan şeyler

| Ne | Komut | Kanıt |
|---|---|---|
| ART açılıyor, canlı `JavaVM` | `pengudroid art-test --sysroot <tree>` | `JNI_CreateJavaVM returned JNI_OK`, `GetVersion = 0x00010006` |
| Gerçek APK DEX'i yükleniyor | `art-test ... --classpath app.apk --find-class <C>` | sınıf *system class loader*'dan çözülüyor |
| Framework native'leri (150 grup) | `art-test ... --register-natives libandroid_runtime.so` | `registered 150 of 150` |
| ActivityThread.main çağrılıyor | `art-test ... --main android.app.ActivityThread` | "invoking ... main" |
| **Uygulama bağlanıyor (`handleBindApplication`)** | `art-test ... --classpath app.apk --bind-app --main android.app.ActivityThread` | `apphost: handleBindApplication returned normally`; `Application`, `LoadedApk`, `GraphicsEnvironment`, `NetworkSecurityConfig` kuruluyor |
| Sistem font haritası | `PENGUDROID_FONTMAP=1 art-test ...` | `--fontmap: 33 families` (fonts.xml'den 158 aile / 367 font) |
| Bundle açma | `pengudroid extract app.apkm --dest D --abi arm64-v8a` | base + split + OBB yazılıyor |
| Bundle analizi | `pengudroid info/doctor app.apkm` | split'lerdeki 37 kütüphane, ABI listesi, DEX sınıf sayısı |
| x86_64 NativeActivity oyunu | `pengudroid native --lib fixtures/native-game/libmain.so` | 1280×720, ~56 fps (RTX 3080) |

Testler: **167 geçiyor** (`cargo test --workspace`).

## Çalışmayanlar ve tam olarak nerede durduğu

### NewPipe 0.29.1 (`org.schabi.newpipe`) — AÇILMIYOR (Activity başlamıyor)

**Güncel durum:** ART açılıyor, `ActivityThread.main` çalışıyor ve
`handleBindApplication` **tamamlanıyor** (yukarıdaki bölüm). Eksik olan tek şey
`system_server`'ın `scheduleLaunchActivity` çağrısı: uygulama bağlı ama hiçbir
Activity başlamadığı için pencere açılmıyor. Aşağısı bu noktaya gelene kadarki
kayıt.

Bu oturumda `NoClassDefFoundError` zinciri çözüldü. `ActivityThread.main`
artık çalışıyor ve **binder'a bağlanma noktasına** kadar geliyor:
```
art: registered 150/150 (0 missing)
art: invoking android.app.ActivityThread.main with 0 argument(s)
[pengudroid:log] ERROR ProcessState: Binder driver /dev/binder is unavailable...
[pengudroid:log] FATAL ProcessState: Binder driver '/dev/binder' could not be opened
```

Çözülen üç ayrı kök neden:

1. **`/apex` ve `/system` yolları host'ta yok.** ART boot classpath'i *konum*
   olarak kaydeder (`/system/framework/framework.jar`, `/apex/...`) ve Java
   tarafındaki `VMClassLoader` bu jar'ları bu adlarla açmaya çalışır. Shim artık
   `PENGUDROID_ROOT` verildiğinde `/apex`, `/system`, `/vendor`, `/product`,
   `/odm`, `/data`, `/system_ext` altındaki yolları o ağaca yönlendiriyor
   (`open`/`openat`/`fopen`/`stat`/`access`/`readlink`/`opendir`/`realpath`).
   Böylece 12 boot jar'ının tamamı açılıyor — chroot/mount namespace yok.
2. **Boot classpath eksikti.** Android 13'te connectivity, bluetooth,
   conscrypt, permission, tethering, wifi, mediaprovider, ... sınıfları mainline
   APEX modüllerine taşındı ve her APEX kendi
   `etc/classpaths/bootclasspath.pb` parçasını yayınlıyor. `Layout::android`
   artık bu parçaları okuyup imajın 12 bileşeninden **sonra** ekliyor (31 jar).
   Eksikken `android.net.ProxyInfo`/`ConnectivityManager`,
   `com.android.org.conscrypt.*`, `android.bluetooth.*` boot loader'da
   bulunamıyordu.
3. **`AndroidRuntime::setArgv0` null `gCurRuntime`.** `ActivityThread.main`
   `Process.setArgV0` çağırıyor; native tarafı `AndroidRuntime::getRuntime()`
   (app_process kurar, bizde yok) sonucunu `setArgv0`'a veriyor ve fonksiyon
   `this->mArgv0` üzerinden null deref yapıyordu. Shim bu sembolü (PLT üzerinden
   geliyor) interpose edip yalnızca thread adını ayarlıyor.

**Binder köprüsü — ÇALIŞIYOR (ilk aşama).**

- Shim `/dev/binder` (ve `hwbinder`/`vndbinder`) için `memfd` tabanlı bir aygıt
  sunuyor; `BINDER_VERSION` → 8, `SET_MAX_THREADS`/`SET_CONTEXT_MGR`/`THREAD_EXIT`
  kabul, `mmap` çalışıyor (shim `PROT_WRITE` ekliyor, çünkü yanıtları aynı
  eşlemeye yazıyor).
- `BINDER_WRITE_READ` yazma tamponunu ayrıştırıyor (`BC_TRANSACTION`,
  `BC_FREE_BUFFER`, ref-count komutları, `BC_ENTER_LOOPER`) ve her senkron
  transaction için `BR_TRANSACTION_COMPLETE` + `BR_REPLY` üretiyor. Yanıt
  parcel'ı libbinder'ın transfer alanına (mmap) yazılıyor.
- Hizmet yöneticisi (`handle 0`) `checkService`/`getService` çağrıları
  yanıtlanıyor: parcel = AIDL durum sözcüğü (0) + `flat_binder_object`
  (tip `0x73682a85`, handle) + **stability** alanı. Handle'lar isme göre sabit.

İki ince ABI ayrıntısı keşfedilerek çözüldü (ikisi de artık kodda yorumlu):

1. `flat_binder_object.type` `0x73682a85`'tir (`B_PACK_CHARS('s','h','*',0x85)`),
   `0x…0002` değil; değer libbinder'ın kendi karşılaştırmalarından okundu.
2. Nesneden **hemen sonra** bir stability alanı gelir ve
   `Parcel::finishUnflattenBinder` onu okur. libbinder yalnızca `3` (vendor),
   `0xc` (system), `0x3f` bitmask'lerini kabul eder; `0` → "Can only set known
   stability", `3` → "Cannot do a user transaction on a vendor stability
   binder ... in a system stability context". Doğru değer **`0xc`**.

Sonuç:
```
art: registered 150/150 (0 missing)
art: invoking android.app.ActivityThread.main
[pengudroid:log] ERROR ProcessState: Binder driver /dev/binder is unavailable...
(12 saniye boyunca canlı kalıyor; rc=124 = timeout)
```
Yani `ServiceManager.getService("activity")` non-null bir `BinderProxy`
döndürüyor, `IActivityManager.attachApplication` transaction'ı kabul ediliyor ve
`ActivityThread.main` `attach()`'i tamamlayıp **`Looper.loop()`'a giriyor** —
gerçek bir app süreci gibi, `system_server`'dan `bindApplication` bekleyerek.

**Sıradaki adım:** uygulamayı gerçekten başlatmak için sunucu tarafını taklit
etmek: `attachApplication` sonrası `bindApplication` çağrısını (uygulama
bilgisi parcel'ı ile) uygulamaya iletmek ve AMS/PMS'in temel sorgularını
(`getPackageInfo`, `getActivityInfo`, ...) yanıtlamak. Bu, `system_server`
semantiğinin başlangıcı; daemon'daki kullanıcı-uzayı broker'ı burada devreye
girecek.

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
- `.toolchain/sysroot/` — LineageOS 20 (Android 13) x86_64 sistem ağacı.
  Yeniden üretmek için (Waydroid imajından, sudo gerekmez):
  ```sh
  mkdir -p .toolchain/sysroot
  /sbin/debugfs -R "rdump /system $PWD/.toolchain/sysroot" \
      /var/lib/waydroid/images/system.img
  python3 tools/art/sysroot-relink.py .toolchain/sysroot   # kırık /apex symlink'leri
  ```
- `.toolchain/armrun.sh` — çevirici için gereken bwrap mount namespace'i.
- `.toolchain/arm-hello` — minimal ARM64 statik ikili (section header'lı).
- `.toolchain/libarmtest.so` — `arm_answer()` → 42 döndüren elle üretilmiş ARM64 `.so`.
- `.toolchain/ndk/x86_64/system.img` — Android 11 x86_64 imajı (sparse değil; ext4
  offset `0x300000`; `EXT4_OFFSET=3145728 python3 .toolchain/ext4.py ...`).
- `.toolchain/ndk-arm/` — çıkarılmış çevirici + ARM runtime.

## Depodaki ART teşhis araçları
- `tools/art/jni_probe.c` — `libart.so`'yu dlopen edip `JNI_CreateJavaVM` çağıran
  minimal sonda (Bionic shim'in `fprintf`'ini atlamak için `write(2)` kullanır).
- `tools/art/oat_kv.py` — OAT key-value store'unda tek bir anahtarı yeniden yazar
  (`apex-versions` boş değere çekmek için).
- `tools/art/sysroot-relink.py` — çıkarılmış ağaçtaki mutlak symlink'leri göreli
  yapar ve `apex -> system/apex` bağını kurar.
- `tools/art/findclass_bp.c` — ptrace ile `ClassLinker::FindClass` /
  `jni::FindClass` çağrılarını ve çökme yerini (modül + sembol ofseti) loglar.
  `PENGUDROID_DEBUG_WRAP="/tmp/findclass_bp <sym>"` ile `art-test`'e takılır.

## `art-test` teşhis anahtarları
- `--vm-option <opt>` — ART'a rastgele seçenek geçir (`-verbose:class`, ...).
- `PENGUDROID_TRACE_FINDCLASS=1` — JNI tablosundaki `FindClass` slotunu sarar ve
  her çağrının sonucunu (ok/MISS) yazar.
- `PENGUDROID_DEBUG_WRAP` — iç `__art` sürecini bir sarmalayıcı ile başlatır
  (ptrace aracı gibi).

## Bu oturumun son commit'leri
`31967a4` binder: transaction'ları yanıtla; ActivityThread.main Looper.loop()'a ulaşıyor ·
`968a57c` shim: /dev/binder'ı süreç-içi aygıt olarak sun ·
`0a82e39` ARM bridge sinyal handler'ları + bulgular · `bfb3675` bundle desteği ·
`02e6c1a` çevirici çalıştırma dokümanı · `cd41f2e` ART JNI_OK + sinyal ABI +
launcher.

## Yeni makinede ilk adımlar
1. `cargo build --workspace` (sistemde Rust yoksa `.toolchain` yeniden kurulmalı).
2. `cargo test --workspace` → 167 test.
3. `.toolchain/sysroot` yoksa yukarıdaki `debugfs` + `sysroot-relink.py` komutları.
4. ART'ı doğrula:
   ```sh
   target/release/pengudroid art-test --sysroot .toolchain/sysroot \
       --register-natives libandroid_runtime.so --main android.app.ActivityThread
   ```
   Beklenen: `registered 150/150`, `invoking android.app.ActivityThread.main`,
   ardından `ProcessState` binder hatası (yani ART tarafı sağlam).
5. Sıradaki iş: binder köprüsü (yukarıdaki NewPipe bölümü).
6. Çevirici için: `docs/LIMITATIONS.md` §2.2 komutu, sonra misafir ARM ART sorunu.
