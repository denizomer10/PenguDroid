# Durum notu (devir teslim)

Bu dosya, kaldığımız yerin dürüst özeti. Başka bir makinede devam edecek kişi
için yazıldı. Ayrıntılı teknik gerekçeler `docs/LIMITATIONS.md` §2.1–2.4.

## Bu oturum: codec listesi çalışıyor

`MediaCodecList` artık iki codec görüyor ve `MediaCodecInfo` nesnelerini
dolduruyor:

```
media: MediaCodecList.getCodecCount() = 2
media: getCodecInfos() returned 2
media:   codec 0: c2.android.avc.decoder
media:   codec 1: OMX.google.h264.decoder
```

Bunu mümkün kılan üç ölçüm:

1. **`createComponent` (kod 1) yanıtı `[status][flat_binder_object]`**, nesne
   offset 4'te. Daha önce bir padding kelimesi eklenmişti; istemci o zaman
   "read object at offset 4" diyordu — yani padding *yanlıştı*.
2. **`listComponents` (kod 3) yanıtı `[hidl status][codec2 status][vec struct]
   [eleman dizisi]…`** — yani **iki** durum kelimesi var. İkinci kelime
   `V1_0::Status` ve doğrudan geri çağrıya geçiyor; sıfır olmazsa
   `Codec2Client::_listComponents` "call failed: BAD_STATE" diyor. Vektörün
   eleman sayısı bu kelimeden değil, 16 baytlık vec yapısının `+8` alanından
   okunuyor (gdb ile doğrulandı: `Status&&` = 1, `vec` = `{ptr, 1}`).
3. **`ComponentTraits` yerleşimi**: `name`@0x00, `mediaType`@0x20,
   `aliases`@0x30 (eleman boyu 16, `hidl_vec` boyu 16). `domain/kind/rank`
   `0x10`'da ama parcel'den okunmuyor, tampondan geliyor.

`getCodecInfos()` bir **örnek** metodu (`new MediaCodecList(ALL_CODECS)`),
statik değil — sonda bunu statik çağırdığımız için `NoSuchMethodError`
alıyorduk.

### Sıradaki duvar: gerçek codec2 servisini barındırmak

`crates/pengudroid-shim/src/codec2.rs` gerçek servisi süreç içinde kuruyor
(`C2PlatformComponentStore` → `utils::ComponentStore` → `BnHwComponentStore`)
ve `interfaceChain` doğru yanıtlanıyor (164 bayt, 4 nesne). Ama yanıt parcel'i
gömülü tamponlarını servisin **kendi yığınına** işaret ettiriyor ve üretilen
kod yanıtı ancak `std::function<void(Parcel&)>` geri çağrısı içinde teslim
ediyor. Rust'tan libc++ `std::function` kurulamadığı için tamponlar
`onTransact` dönmeden ölüyor:

```
hw-Parcel: Buffer length 32 does not match expected size 15638207488
HidlServiceManagement: getService: received incompatible service
```

Bu yüzden barındırma `PENGUDROID_CODEC2_SERVICE=1` ile **opt-in**; varsayılan
yol eski stub (codec listesi çalışıyor).

Çözüm yolları, kolaydan zora:

1. `std::function<void(Parcel&)>` için libc++ `__func` yerleşimini elle kur
   (nesne 48 bayt: `{tablo@0, yakalananlar@8..0x30}`; tablo
   `libhidlbase`'de `0xcd160`'ta, `__clone()` `0xa26f0`, `__clone(void*)`
   `0xa2730`). Tablo slot sırası henüz kesinleşmedi.
2. `BnHwComponentStore::onTransact` yerine `utils::ComponentStore`
   metodlarını doğrudan çağır ve geri çağrıyı aynı şekilde kur.
3. Gerçek bir binder **sunucusu** yaz (daemon'da), servisi ayrı süreçte
   çalıştır; o zaman sürücünün tampon kopyalama işini de biz yaparız.

### Parcel yerleşimi (ölçüldü, `libhidlbase`)

```
0x28  u8*  mData        0x30  size_t mDataSize    0x38  size_t mDataCapacity
0x40  size_t mDataPos   0x48  size_t* mObjects   0x50  size_t mObjectsSize
0x58  size_t mObjectsCapacity   0x60  size_t mNextObjectHint
```

`setData` (`0xb9d80`) tamponu 0x28'e, boyu 0x30'a yazar; `readObject<T>`
(`0xb8ca0`) 0x40/0x48/0x50'yi okur. Bir istek parcel'ini yeniden kurmak için
`Parcel()` + `setData` + bu alanları elle doldurmak yetiyor.

## Önceki oturum: MediaCodec — HIDL yolu açıldı

Hedef: `MediaCodecList` codec görsün, `MediaCodec` kare üretsin. Bu oturumda
zincirin **taşıma katmanı** tamamen çalışır hale geldi; codec2 istemcisi
kuruluyor, `listComponents` yanıtı kaldı.

Ölçülen son durum:

```
Codec2Client: Available Codec2 services: "default"
Codec2Client: Client to Codec2 service "default" created
[binder]   -> codec2 call code 8      (getConfigurable)
[binder]   -> codec2 call code 2      (getPoolClientManager)
[binder]   -> codec2 call code 256067662  (interfaceChain)
[binder]   -> codec2 call code 3      (listComponents)
media: MediaCodecList.getCodecCount() = 0
```

### Sıradaki somut adım: `ComponentTraits` yerleşimi

`Codec2InfoBuilder::buildMediaCodecList` (`frameworks/av/media/codec2/sfplugin/
Codec2InfoBuilder.cpp`) şunları yapıyor:

```cpp
std::vector<Traits> traits = Codec2Client::ListComponents();
for (const Traits& trait : traits) {
    std::vector<std::string> nameAndAliases = trait.aliases;   // ← vec<string>
    nameAndAliases.insert(nameAndAliases.begin(), trait.name);
    for (name : nameAndAliases) {
        auto intf = Codec2Client::CreateInterfaceByName(name, &client);  // ← createInterface
        if (!intf) { ALOGD("could not create interface for %s"); continue; }
        if (parser.getCodecMap().count(name) == 0) { ... }        // ← XML'de olmalı
    }
}
```

Yani `listComponents` yanıtındaki her trait'te `name`, **`aliases` (vektör)**,
`domain`, `kind`, `rank`, `mediaType` olmalı; ardından istemci
`createInterface(name)` çağırıyor ve codec'in `media_codecs.xml`'de bir kaydı
olması gerekiyor (`c2.android.avc.decoder` orada var).

Şu anki yanıt `aliases`'i tek string olarak yazıyor; doğru yerleşim
`android.hardware.media.c2@1.0.so`'daki
`_hidl_listComponents`'ın dönüş yazma kodundan çıkarılmalı.

**Ölçüldü** (`writeEmbeddedToParcel(ComponentTraits, ...)`, `0x5de70`):

```
0x00  hidl_string   (name)
0x10  hidl_string   (mediaType)
0x20  u32 domain, u32 kind, u32 rank, u32 pad
0x30  hidl_vec<hidl_string> aliases   (ptr @0x30, size @0x38)
0x40  hidl_string   (?)               ← üçüncü string
```

Yani struct **80 bayt** ve `aliases` 0x30'da bir vektör. Sıradaki iş bu
yerleşimi `push_buffer_object` çağrılarıyla yazmak: vec struct (16), eleman
dizisi (80), sonra her string/vektör için gömülü nesneler.

## Önceki oturum: NewPipe ekranda — çizim ve sunum çalışıyor

### Yol boyunca bulunan dört gerçek hata

1. **Binder komutları ioctl-kodlu.** `BC_TRANSACTION` = `0x40406300`,
   `BC_ACQUIRE` = `0x40046302`, … ve **yük boyutu komut sözcüğünün 16–30.
   bitlerinde**. Elle yazılmış bir tablo (`BC_ACQUIRE => 4` gibi) bir komut
   farklı bir argüman tipiyle gelince sessizce kayıyordu. Ayrıca libbinder
   `BC_TRANSACTION` değil **`BC_TRANSACTION_SG`** (`0x40486311`, 72 bayt)
   gönderiyor; bu tanınmadığı için HIDL transaction'ı hiç işlenmiyordu.
   Çözüm: `command_payload` artık boyutu komut sözcüğünden okuyor.
2. **`libhidlbase` HIDL'i `access("/dev/hwbinder")` ile kapılıyor.** Sanal
   cihaz host'ta olmadığı için `ENOENT` dönüyordu ve her HIDL araması
   "Hardware service manager is not running" ile ölüyordu. Shim artık
   `/dev/binder`, `/dev/hwbinder`, `/dev/vndbinder`, `/dev/ashmem` için 0 diyor.
3. **`hwservicemanager.ready` özelliği.** `libhidlbase` bu özellik "true"
   olana kadar bekliyor; varsayılan özellik tablosuna eklendi.
4. **HIDL servis yöneticisi yanıtları.** Broker artık handle 0'da `ping`,
   `interfaceChain`, `get`, `getTransport` ve `listByInterface` çağrılarını
   yanıtlıyor. Java `Parcel` ile HIDL parcel'ı, parcel'ın 8. baytındaki `TSYS`
   interface token'ından ayırt ediliyor.

### Codec kütüphanesi süreç içinde yükleniyor

`libcodec2_soft_avcdec.so` (APEX'teki yazılım H.264 çözücü) artık `dlopen`
ediliyor. Bunun için `AHardwareBuffer` shim'ine beş giriş noktası eklendi:
`getNativeHandle`, `createFromHandle`, `lock`, `unlock`, `lockPlanes`,
`isSupported`. Handle gerçek bir gralloc0 `native_handle_t` (fd + stride/format/
width/height/usage).

Denemek için (sysroot'un tüm kütüphane kümesini de-bionic'leyip yükler):

```sh
pengudroid nativelib .toolchain/sysroot/system/lib64/libcodec2_soft_avcdec.so \
  --sysroot .toolchain/sysroot
```

### Sıradaki adım: `ICodec2` servisi

`Codec2Client` önce VINTF manifest'ine bakıyor ("No Codec2 services declared in
the manifest"), sonra `listByInterface` ile servis arıyor. Yapılacaklar:

1. `listManifestByInterface("android.hardware.media.c2@1.0::IComponentStore")`
   → `["default"]`. Çağrı **kod 5** olarak geliyor (`Codec2Client::GetServiceNames`
   `IServiceManager1_2` üzerinden yapıyor) ve yanıt `vec<hidl_string>`
   (instance adları). Şu an `[status u32][count u64][40 baytlık struct + gömülü
   string]` yazılıyor; istemci hâlâ boş liste görüyor — **yanıtın yerleşimi hâlâ
   yanlış**.

   Ölçülen istek yerleşimi: parcel = `[interface token][40 baytlık hidl_string
   struct][gömülü string verisi]`; her argüman iki 40 baytlık blok.

   **Yanıt tarafı disassemble edildi** (`libhidlbase` `0x9a8f0`, `transact`'a
   verilen lambda):

   ```
   readFromParcel(Status)                     → 4 bayt
   readBuffer(0x10, &parentOffset, &ptr)      → 16 baytlık hidl_vec struct
   size = *(u32*)(ptr + 8)                    ← sayaç u32, offset 8
   readNullableEmbeddedBuffer(size * 16, 0, parentOffset, 0, &arr)
   for i in 0..size:
       readEmbeddedFromParcel(hidl_string, parcel, parentOffset, i * 16)
   ```

   **Asıl mekanizma bulundu:** HIDL parcel'ında string'ler düz bayt değil,
   **`binder_buffer_object`** kayıtlarıdır (`readEmbeddedBuffer`, `0xbc3f0`):

   ```cpp
   struct binder_buffer_object {   // 40 bayt
       uint32_t type;        // 0x70742a85
       uint32_t flags;       // 0 = normal, 1 = gömülü
       uintptr_t buffer;     // veri işaretçisi
       size_t length;
       size_t parent;        // üst nesnenin handle'ı
       size_t parentOffset;
   };
   ```

   `writeEmbeddedBuffer` (`0xbae20`) bu yapıyı parcel'ın **veri alanına** yazıp
   konumunu **offsets dizisine** ekliyor; okuyucu `readObject<T>(handle)` ile
   offsets dizisinden buluyor. Yani yanıt için broker'ın `Reply.objects`
   listesine bu nesnelerin offset'lerini eklemesi gerekiyor — şu an sadece
   Java `flat_binder_object` handle'ları yazılıyor, bu yüzden HIDL yanıtı
   okunmuyor.

   Sıradaki somut iş: `make_reply`'in HIDL dalında (a) vec struct'ı,
   (b) eleman dizisini, (c) her string verisini `binder_buffer_object` olarak
   yazmak ve `parent`/`parentOffset` alanlarını doğru doldurmak.
2. `get(...)` → `codec2` handle'ı (hazır).
3. `ICodec2::listComponents` → `ComponentInfo` listesi.
4. `ICodec2::createComponent` + `ICodec2Component` (`configure`, `start`,
   `queue`, `dequeue`, `setOutputSurface`) ve `ICodec2ComponentListener`
   geri çağrıları (broker'ın uygulamaya transaction göndermesi gerekir;
   `APP_THREAD` altyapısı bunun için var).

Not: sysroot'un `system/etc/vintf/manifest.xml` dosyasına elle
`@1.0::ICodec2/default` eklendi (yerel artefakt, git'te değil). Manifest'in
hiç açılmadığı `PENGUDROID_TRACE_ROOTFS=1` ile görüldü — yani sorun manifest
değil, `listManifestByInterface` yanıtı.

Çıktı tarafı ayrı bir iş: `SurfaceView` ayrı bir yüzey istiyor, bizim
compositor'ımızda katman yok. İlk hedef çözülen kareyi `PENGUDROID_DUMP_FRAME`
benzeri bir yolla diske yazıp doğrulamak.

## Önceki oturum: NewPipe ekranda — çizim ve sunum çalışıyor

**`pengudroid launch NewPipe_v0.29.1.apk` bir Wayland penceresi açıyor ve
NewPipe'ın canlı akışını gerçek verilerle çiziyor.** Kareler hwui tarafından
host GPU'da üretiliyor, dma-buf olarak ihraç ediliyor, daemon onu
`zwp_linux_dmabuf_v1` ile içe alıp commit ediyor. Ölçüm: 20 saniyede ~250 kare,
`FATAL`/`SIGSEGV` yok.

![NewPipe](screenshots/newpipe.png)

### Girdi (input) da çalışıyor

Daemon `wl_seat`'i bağlıyor, pointer/keyboard olaylarını yüzeyin sahibine
iletiyor; apphost bunları `MotionEvent`'e çevirip
`ViewRootImpl.dispatchInputEvent` ile uygulamaya veriyor. Bir tıklama NewPipe'ın
video sayfasını açıyor:

![NewPipe video sayfası](screenshots/newpipe-video.png)

Videoyu *oynatmak* hâlâ mümkün değil: oynatıcı `MediaCodec` istiyor, bizde
`codec2` servisi yok (bkz. aşağıdaki sınırlar).

Ekran görüntüsü alınamayan oturumlarda girdiyi denemek için:
`PENGUDROID_SYNTH_CLICK=400,400` (8 saniye sonra bir dokunuş enjekte eder).

### Sıradaki durak: `MediaCodec` (video oynatma)

Oynatıcı açılıyor ama kare üretemiyor. Zincir şu:

1. `MediaCodecList` → `libmedia_jni` → `MediaCodecList::getInstance()` →
   `Codec2InfoBuilder` → `Codec2Client::CreateFromService("default")`.
2. Servis, swcodec APEX'indeki `android.hardware.media.c2@1.2-service`
   (Bionic bir binary; host'ta çalıştıramıyoruz).
3. Servis yoksa liste boş dönüyor ve ExoPlayer "no decoder" diyor.

İki yol var:

- **Doğru yol:** broker'a HIDL `ICodec2` servisini eklemek ve arkasında
  `libcodec2_soft_avcdec.so`'yu (APEX'te var, `createCodec2Factory` ihraç
  ediyor) süreç içinde yüklemek. HIDL parcel'ı, buffer queue'ları ve
  `C2Component` yaşam döngüsü yazılmalı.
- **Kısa yol:** boot class path'in *önüne* kendi `android.media.MediaCodec`
  sınıfımızı koyup host'un libavcodec'ine bağlamak. Daha az kod ama
  framework'ün geri kalanıyla (`MediaExtractor`, `MediaFormat`, surface
  çıkışı) uyum riski yüksek.

Ses tarafı da aynı şekilde eksik: `AudioTrack` → `AudioFlinger` yok, ama
daemon'ın PipeWire ring'i hazır (`audio-test --via-daemon`).

```sh
pengudroid daemon &
PENGUDROID_ART_SYSROOT=.toolchain/sysroot \
  pengudroid launch NewPipe_v0.29.1.apk
```

### Neydi tıkanıklık: `relayout` yanıtında pencere çerçevesi yoktu

Üç ayrı hata üst üste biniyordu. Sırayla:

1. **`IWindowSession.relayout` yanıtı `outFrames`'i "yok" olarak yazıyordu.**
   `ViewRootImpl.mTmpFrames` önceden ayrılmış 0×0 değerinde kalıyordu, yani
   `mWidth`/`mHeight` sıfırdı. `performDraw` **çağrılıyordu** ama `draw()`
   içindeki `dirty` dikdörtgeni boş olduğu için hwui'ye hiç inmiyordu.
   Çözüm: gerçek bir `ClientWindowFrames` yazmak. LineageOS 20'nin
   `readFromParcel`'ı `[frame][displayFrame][parentFrame][attachedFrame][bool]
   [float]` sırasını okuyor (AOSP 13.0.0_r1'de `attachedFrame`/`compatScale`
   yok — sürüm farkı burada ısırıyor).
2. **`eglSetDamageRegionKHR` shim'de yoktu.** hwui her swap öncesi çağırıyor ve
   başarısız olunca `LOG_ALWAYS_FATAL` ile ölüyordu. Eklendi; ayrıca
   `eglGetProcAddress` artık kendi uyguladığımız giriş noktalarını host'tan önce
   döndürüyor.
3. **hwui varsayılan framebuffer'a (id 0) çiziyor.** Surfaceless bir host
   context'te FBO 0 hiçbir yere gitmiyor. `glBindFramebuffer` interpose edildi:
   `0` bağlanınca o anki yüzeyin FBO'su bağlanıyor. Piksellerin görünmesini
   sağlayan asıl değişiklik bu.

Ek olarak `relayoutWindow`'un `mSurfaceControl.isValid()` false olduğunda
`destroySurface()` çağırıp bizim geçerli `mSurface`'ımızı da geçersiz kıldığı
görüldü; `mSurfaceControl` de framework'ün kendi `SurfaceControl.Builder`'ı ile
kuruluyor ve `mForceDisableBLAST` açılıyor (BLAST yolu gerçek bir consumer
istiyor).

### Yeni teşhis araçları

- `PENGUDROID_TRACE_DRAW=1` — ana thread `Looper.loop()` içindeyken ayrı bir host
  thread'i `ViewRootImpl` alanlarını yokluyor: `mAttachInfo.mDrawingTime`,
  `mDirty`, `mIsDrawing`, `mWidth/mHeight`, `mSurface.isValid()`.
  `mDrawingTime` sıfırdan farklıysa `draw()` gerçekten çalışmıştır.
- `PENGUDROID_DUMP_FRAME=/tmp/f.ppm` — 200/201/400. kareleri `glReadPixels` ile
  okuyup PPM yazar. Ekran görüntüsü alınamayan oturumlarda çizimin kanıtı budur.
- `PENGUDROID_TRACE_GLESHIM=1` artık `eglSurfaceAttrib`, `ANativeWindow` boyutu
  ve `eglCreateWindowSurface` sonucunu da yazıyor.

### Bilinen sınırlar (bu oturumda değişmedi)

- `-Xint` zorunlu: JIT `nterp_op_invoke_virtual` içinde çöküyor.
- Yüzey boyutu 1280×720 (yatay) sabit; `ANativeWindow_getWidth/Height` shim'den
  geliyor, ekran modu da 1280×720.
- **Video oynatma yok**: `MediaCodec`/`codec2` servisi yok, `MediaCodecList`
  boş dönüyor. Ses (`AudioTrack`/`AudioFlinger`) de yok.
- Klavye olayları Linux keycode'u olarak geliyor; Android keycode eşlemesi
  henüz yok (pointer çalışıyor).
- Beach Buggy Racing 2 / Genshin Impact: ARM çevirici guest kodu
  çalıştıramıyor (`docs/LIMITATIONS.md` §2.1).

## Önceki oturum: NewPipe hwui ile çiziyor (70+ sn stabil)

Gralloc duvarı aşıldı. `art-test ... --bind-app` ile NewPipe **70 saniye
boyunca canlı kalıyor**, `FATAL`/`SIGSEGV` yok, `Choreographer` kareleri işliyor
ve hwui 1280×720 RGBA_8888 buffer'larını bizim ayırıcımızdan alıyor:

```
[ahb] allocated 1280x720 fmt=1 stride=1280 (3686400 bytes)   ×6
```

### Nasıl çözüldü

`GraphicBufferMapper`/`GraphicBufferAllocator` kurucularındaki
`LOG_ALWAYS_FATAL`'lar interpose edildi, sonra **`GraphicBufferAllocator::allocate`**
shim'de uygulandı. Kritik ayrıntılar (hepsi gdb ile ölçüldü):

1. `allocate` bir **üye fonksiyon**: `rdi` = `this`, sonra `rsi=w, rdx=h,
   rcx=format, r8=layerCount, r9=usage`.
2. Mangled isimdeki `Pjm` iki farklı şey: `outStride` bir **pointer** (`Pj`),
   sonraki argüman ise **değer olarak `unsigned long`** (`m`). Ona pointer gibi
   yazmak SIGSEGV veriyordu.
3. `GraphicBufferMapper::getTransportSize` sembolü `16getTransportSize…PjS4_`
   (uzunluk 16, `S4_`); yanlış yazım sessizce host'a düşüyordu.

Buffer'lar memfd + klasik gralloc0 `native_handle` düzeniyle ayrılıyor
(`data[0]=fd`, sonra stride/format/width/height/usage).

### Kalan iş: sunum

Pikseller bizim memfd buffer'larında; **Wayland'a sunum henüz yok** (daemon
logunda pencere yok). Ölçülenler:

- `PENGUDROID_TRACE_GLESHIM=1` ile görüldü ki **`eglSwapBuffers` hiç
  çağrılmıyor** — hwui kareyi bitirmiyor. Buna karşılık
  `eglCreateWindowSurface` **iki kez** çağrılıyor, yani hwui bizim offscreen
  FBO yüzeyimizi kuruyor ama kareyi swap etmiyor.
- Broker'a `graphic_buffer_producer` handle'ında **hiç transaction gelmiyor**
  (`queueBuffer` yok).
- `BufferQueueProducer::allocateBuffers` 6 buffer ayırıyor (bizim memfd'ler),
  yani BLASTBufferQueue'nin yerel producer'ı çalışıyor ama consumer'ı bizim
  `gbp`'mize bağlanmıyor.

Sıradaki adım: BLASTBufferQueue'nin consumer'ını bizim `gbp`'mize bağlayan yolu
(`ISurfaceComposer`/`setConsumer` ya da `createSurface` yanıtındaki gbp'nin
BQB tarafından kullanılması) çözmek; ardından `queueBuffer`'da gelen buffer'ı
`zwp_linux_dmabuf_v1` (veya `wl_shm`) ile compositor'a vermek.

### Çizim neden hiç başlamıyor (bu turun ölçümü)

- Render thread **başlıyor** (`RenderThread::threadLoop` hit) ama
  `CanvasContext::draw` ve `RenderProxy::syncAndDrawFrame` **hiç çağrılmıyor**.
- `handleResumeActivity` normal dönüyor, `Looper.loop()`'a giriliyor;
  `ViewRootImpl.performTraversals` çalışıyor (measure/layout görülüyor,
  `relayout` 8 kez çağrılıyor) ama **`performDraw`'a hiç gelinmiyor**.
- Süreç **%1.5 CPU, state S** — yani uygulama boşta bekliyor, kilitli değil.
- `Activity.makeVisible()` + `decorView.invalidate()` açıkça çağrıldı (apphost),
  yine swap yok. Yani `ViewRootImpl.isViewVisible()` false kalıyor
  (muhtemelen `mAppVisible`).

Sıradaki adım: `mAppVisible`'ı true yapan yolu bulmak (cihazda bunu
`WindowManagerService`/`setAppVisibility` yapar) veya `ThreadedRenderer.draw`'ı
doğrudan tetiklemek.

Denenen ve **işe yarayan kısım**: `WindowManagerGlobal.getInstance().mRoots`
üzerinden gerçek `ViewRootImpl`'e ulaşılıyor (2 root var), `mAppVisible` alanı
`true` yapılıyor ve `scheduleTraversals()` çağrılıyor. Bu olmadan
`isViewVisible()` false kalıyor ve `performTraversals` `performDraw`'a hiç
gelmiyordu.

**Önemli bulgu:** `performTraversals()`'ı **doğrudan** çağırmak
`Choreographer.getFrameTimeNanos()` üzerinden
`IllegalStateException: This method must only be called as part of a callback
while a frame is in progress` atıyor; doğru giriş noktası `scheduleTraversals()`.

**Kalan durak:** `scheduleTraversals()` her iki root için de dönüyor, vsync
saati 1424 olay gönderiyor (`dropped=0`), traversal'lar **çalışıyor** (relayout
çağrıları bizim schedule'dan *sonra*), ama `RenderProxy::syncAndDrawFrame`
hâlâ çağrılmıyor.

Bu turda ayrıca ölçüldü ve düzeltildi:

- `ViewRootImpl.mSurfaceControl.isValid()` **false** idi (relayout yanıtındaki
  SurfaceControl parcel'ı bu sürümde ayrıştırılmıyor). Çözüm: SurfaceControl'ü
  framework'ün **kendi** `SurfaceControl.Builder`'ı ile kurmak (bu yol
  `SurfaceComposerClient.createSurface`'a gider ve broker onu yanıtlar) ve
  `mSurface.copyFrom(built)` ile bağlamak → `mSurface.isValid() = true`.
- `mView.getVisibility() = 0` (VISIBLE) doğrulandı.

Yani çizim koşullarının üçü de (visible view, `mAppVisible`, geçerli surface)
sağlanıyor; `performDraw`'ı engelleyen şey artık uygulamanın kendi
`OnPreDrawListener`'ları (feed verisi hazır olana kadar iptal ediyor) gibi
görünüyor.

## Önceki oturum: display bilgisi verildi, hwui çizim yoluna girdi

`ADisplay_acquirePhysicalDisplays`'ın istediği **üç** yanıt tamamlandı ve her
biri ölçülerek doğrulandı:

1. **`getPhysicalDisplayIds`** (AIDL code 3) → `[count][int64...]`.
2. **`getStaticDisplayInfo`** (legacy code 3) → `[status][size][blob]`. Blob,
   `ui::StaticDisplayInfo::unflatten` + `DeviceProductInfo::unflatten`
   disassembly'sinden çıkarıldı: `[4B connected][4B density][1B secure]`
   `[u64 nameLen][u32 year][u64 modelLen][u64][u32][4B]`. **Kritik ayrıntı:**
   istemci `status`'ten sonra bir **boyut** alanı okuyor (`readInt32`,
   `readInt32`, `readInplace(size)`, `unflatten`); boyut yazmadan `unflatten`
   `-ENOMEM` veriyordu.
3. **`getDynamicDisplayInfo`** (legacy code 55) → aynı `[status][size][blob]`
   biçimi. Blob elle yazılmadı: libui'nin **kendi** `flatten`'i sıfırlanmış bir
   `ui::DynamicDisplayInfo` üzerinde çağrılıyor (`getFlattenedSize` +
   `flatten`, ikisi de ihraç edilmiş). `supportedDisplayModes` vektörü boş
   olamıyor (`-ENODEV`), bu yüzden 0x38 baytlık bir `ui::DisplayMode`
   (`id=1`, 1280×720, 60 Hz) vektöre bağlanıyor.

Ayrıca **handle/kod çakışması** çözüldü: aynı handle ve aynı kod numarası iki
farklı arayüze ait olabiliyor. Ayrım artık istek parcel'ındaki **interface
token**'a göre yapılıyor (`android.ui.ISurfaceComposer` = legacy,
`android.gui.ISurfaceComposer` = AIDL); token 12 baytlık bir ön-ekten sonra
geliyor (`[u32][-1]["TSYS"]`).

Sonuç: WCG abort **yok**, `DeviceInfo::updateDisplayInfo` çökmesi **yok**.
Yeni durak: `OpenGLRenderer: Unreachable: unsupported pixel format: 0`.

### Pixel format duvarı — AŞILDI

`DeviceInfo::updateDisplayInfo` (libhwui) şu zinciri çalıştırıyor:

```
ADisplay_acquirePhysicalDisplays
  → ADisplay_getDisplayType            (0 = physical)
  → ADisplay_getPreferredWideColorFormat(&dataspace, &format)
      format 1 (RGBA_8888) veya 0x16 olmalı, yoksa assert
```

`ADisplay_getPreferredWideColorFormat` yalnızca `ADisplay` nesnesinin
`+0x0c` (dataspace) ve `+0x10` (format) alanlarını kopyalıyor. gdb ile ölçüldü:
bu iki alan `getCompositionPreference`'ın (legacy code **27**, istemci 5 adet
`readInt32` okuyor) **3.** ve **4.** yüklerinden geliyor. 4. yüke `1`
(RGBA_8888) verilince assert **kayboldu**.

### Yeni durak: gralloc mapper

```
WARN  Gralloc2: mapper 2.x is not supported
FATAL GraphicBufferMapper: gralloc-mapper is missing
```

hwui `AHardwareBuffer`/`GraphicBuffer` ayırmak için gralloc mapper'ı arıyor;
shim'de yok. Ölçülen zincir:

```
HidlServiceManagement: getService: defaultServiceManager() is null
Gralloc4/3/2: mapper N.x is not supported
FATAL GraphicBufferMapper: gralloc-mapper is missing
```

Mapper'lar **HIDL** (`android.hardware.graphics.mapper@2.0/3.0/4.0`), yani
`hwservicemanager` üzerinden aranıyor; bizim broker yalnızca normal servis
yöneticisini taklit ediyor.

Bu turda `crates/pengudroid-shim/src/ahardwarebuffer.rs` eklendi: hwui'nin
kullandığı beş giriş noktası (`allocate`, `describe`, `acquire`, `release`,
`to_ANativeWindowBuffer`) memfd tabanlı bir `AHardwareBuffer` ile
uygulanıyor. **Ancak ölçüm şunu gösterdi:** `AHardwareBuffer_allocate` hiç
çağrılmıyor (`PENGUDROID_TRACE_AHB=1` ile doğrulandı); mapper **libgui/libui
içinden** isteniyor (hwui bu sembolü import etmiyor).

Aynı dosyada `GraphicBufferMapper` ve `GraphicBufferAllocator` **kurucuları**
interpose edildi; böylece iki `LOG_ALWAYS_FATAL` ("gralloc-mapper is missing",
"gralloc-allocator is missing") kalktı. Çökme şimdi `allocateHelper` içinde:

```
#0 GraphicBufferAllocator::allocateHelper
#2 GraphicBuffer::initWithSize
#4 BufferQueueProducer::allocateBuffers
```

`allocateHelper` `mAllocator`'ı (`this+0x8`) yükleyip **vtable slot 0**'ı
çağırıyor; `mAllocator` null olduğu için çöküyor. `allocate`'ı interpose etmek
yetmiyor çünkü `allocate → allocateHelper` çağrısı PLT'den geçmiyor.

**Sıradaki adım (net):** `GraphicBufferAllocator::allocate` artık shim'de interpose
ediliyor ve **çağrılıyor** (çökme bizim `.so`'muzun içine taşındı), ama gövde
henüz tamam değil: `allocate_buffer` içinde bir yerde SIGSEGV var. Çağrı
sitesi söküldü: `rdi=nesne, rsi=descriptor, edx=count, ecx, r8, r9d` + yığın
argümanları; `std::string` görünmez referansla (pointer) geçiyor.

Not: `allocateHelper` PLT'den çağrılıyor gibi görünse de (0x501b0) pratikte
libui kendi tanımına bağlanıyor; işe yarayan interpose `allocate` oldu.

`eglQuerySurface` artık `EGL_SWAP_BEHAVIOR` (→ `EGL_BUFFER_DESTROYED`) ve
`EGL_BUFFER_AGE_EXT` (→ 1) yanıtlıyor; "Unable to match the desired swap
behavior" uyarısı bu sayede kalktı.

Ayrıca `pengudroid-gleshim`'e **`eglSurfaceAttrib`** eklendi: hwui her yüzeyde
`EGL_SWAP_BEHAVIOR = EGL_BUFFER_DESTROYED` ayarlıyor ve başarısız olunca abort
ediyordu (`EGL_BAD_SURFACE`, çünkü sembol host EGL'ye düşüyordu).

## Önceki oturum: WCG duvarı aşıldı, hwui EGL'ye geldi

Belgelenen "`getPhysicalDisplayIds` boş dönüyor" durağının kökü **kod numarası
değil, yanıt biçimiydi**:

- `SurfaceFlinger` adı **iki ayrı arayüzü** gösteriyor: eski (hand-written)
  `android.ui.ISurfaceComposer` (istemci "SurfaceFlinger" adıyla istiyor) ve
  AIDL `android.gui.ISurfaceComposer` ("SurfaceFlingerAIDL"). `handle_for` her
  ad için ayrı handle üretiyor, biz ise tek bir literal adla karşılaştırıyorduk.
- Eski arayüzün yanıtlarında **status kelimesi yok**. Biz status yazdığımız için
  istemcinin `readInt64Vector`'ü status'ü (0) eleman sayısı sanıyor, dizi boş
  görünüyor ve hwui "Failed to acquire physical displays for WCG support!" ile
  abort ediyordu.
- Düzeltme: `is_surface_flinger()` iki adı da tanıyor; `reply_has_status()`
  yalnızca AIDL handle'ında status yazıyor. `getPhysicalDisplayIds` kodu **3**
  (`BpSurfaceComposer` içinde `mov $0x3,%esi` ile doğrulandı) ve yanıt
  `[int32 count][int64 id...]`.

Sonuç: WCG abort **kayboldu**, hwui EGL'yi başlatıyor
(`OpenGLRenderer: Failed to initialize 101010-2 format` = zararsız WCG probu) ve
NewPipe Activity'si **launch + resume** oluyor:

```
apphost: handleLaunchActivity returned an Activity
apphost: calling handleResumeActivity
FragmentManager: Added fragment to active set MainFragment
```

Şimdi kalan durak: feed yüklendikten sonra (SQLite sorgusundan hemen sonra)
SIGSEGV. ART'nin kendi fault handler'ı bu ortamda döküm üretmiyor (Bionic
`ucontext_t` bekliyor, glibc farklı düzen veriyor), bu yüzden
`.toolchain/crashbt.c` yazıldı: `PENGUDROID_EXTRA_PRELOAD=<crashbt.so>` ile VM
kurulduktan **sonra** dlopen edilip SIGSEGV/SIGBUS/SIGILL/SIGFPE/SIGABRT için
glibc backtrace basıyor (alt sinyal yığını + `SYS_rt_sigaction` ile, çünkü
shim'in `sigaction`'ı Bionic düzeni bekliyor).

Şimdi kalan durak: **render thread**'de `DeviceInfo::updateDisplayInfo()`
(`EglManager::createSurface` içinden) `displays[0]`'ı null dizide okuyor:

```
#0 DeviceInfo::updateDisplayInfo()  mov (%rax,%rbx,8),%rdi   rax=0
#2 EglManager::createSurface(...)
#8 RenderThread::threadLoop()
```

`ADisplay_acquirePhysicalDisplays` (libnativedisplay) her id için
`getPhysicalDisplayToken` **ve** `getStaticDisplayInfo` çağırıyor. Ölçülenler:

- `SurfaceComposerClient::getPhysicalDisplayIds()` **1 eleman** döndürüyor
  (gdb ile vektörün begin/end'i okundu), yani id tarafı sağlam.
- `getPhysicalDisplayToken` (AIDL code 5) artık `[status][strongBinder]`
  döndürüyor.
- `SurfaceComposerClient::getStaticDisplayInfo` çağrılıyor ve **hata (1)**
  döndürüyor → display atlanıyor → dizi boş → çökme.

`ui::StaticDisplayInfo::unflatten` düzeni söküldü: `[4B connected][4B density]
[1B secure][DeviceProductInfo flattened][4B]`. `DeviceProductInfo`'nun kendi
flattened boyutu en az 12 bayt (`getFlattenedSize` = 8 + string + 4 + …), yani
blob 25+ bayt. Sıradaki adım: legacy `getStaticDisplayInfo` kodunu (legacy
handle'da binder token taşıyan kodlar **3** ve **8**) ve `DeviceProductInfo`
blob'unu doğru üretmek.

Bu turda ayrıca `getPhysicalDisplayToken` (AIDL code 5) `[status][strongBinder]`
olarak yanıtlanıyor ve legacy isteklerde binder token taşıyan kodlar trace'te
loglanıyor.

## Önceki oturum: Roblox Activity'yi resume ediyor

Hedef Roblox (`/tmp/roblox/base.apk` + `split_config.x86_64.apk`). Zincirin
tamamı çalışıyor:

```
JNI_CreateJavaVM → handleBindApplication (Application.onCreate tamam)
                 → handleLaunchActivity (ActivitySplash.onCreate tamam)
                 → handleResumeActivity (window eklendi, onResume tamam)
                 → Looper.loop()
```

Sonra `libroblox.so + 0x224e4af` içinde SIGSEGV. Yani sıradaki duvar
**grafik yığını**: `relayout` gerçek bir `SurfaceControl` döndürmüyor, dolayısıyla
motor çizemiyor.

### Bu oturumda düzeltilen gerçek hatalar

1. **`struct addrinfo` ABI'si (en büyük kazanç).** Bionic `ai_canonname`'i
   `ai_addr`'dan **önce** koyar (0x18 / 0x20), glibc tersini yapar. Shim glibc
   listesini olduğu gibi Bionic koduna veriyordu; `p->ai_addr` aslında
   `ai_canonname` (NULL) okuyor ve `sockaddrToInetAddress` içinde SIGSEGV
   oluyordu. `crates/pengudroid-shim/src/netdb.rs` iki yönlü çeviri yapıyor,
   `AI_*` bayraklarını ve `EAI_*` kodlarını da eşliyor. `getaddrinfo`,
   `freeaddrinfo`, `gai_strerror` de interpose edildi (gerçek glibc'ye
   `dlsym(RTLD_NEXT, …)` ile ulaşılıyor, yoksa sonsuz özyineleme).
2. **FORTIFY semantiği.** Bionic'in `__strncpy_chk2`/`__stpncpy_chk2`'si
   `n > src_len` diye abort **etmez**; yalnızca kopyalama sırasında `src`'nin
   sonunu geçerse abort eder. Biz glibc'nin katı kuralını uyguluyorduk ve
   `libroblox.so` `strncpy(dst, "x", 4078)` yaptığı için süreç ölüyordu.
   `__strncat_chk`, `__strlen_chk`, `__strchr_chk`, `__strrchr_chk` de Bionic'e
   göre düzeltildi.
3. **SDK seviyesi framework'e uyduruldu.** Shim `ro.build.version.sdk=36`
   diyordu ama sysroot Android 13. Uygulamalar API 34+ yollarına girip
   `NoSuchMethodError` alıyordu (Roblox: `JobScheduler.forNamespace`). Artık
   `$PENGUDROID_ROOT/system/build.prop`'tan okunuyor (33).
4. **`Settings` provider'ı bind'den önce kuruluyor.** `RobloxApplication.onCreate`
   `Settings.Global` okuyor; reply `PENGUDROID_DUMP_HOLDER` arkasında ve bind'den
   *sonra* kuruluyordu.
5. **`IContentProvider` gerçek cevaplar.** `query` artık `BulkCursorDescriptor`
   (count=0) döndürüyor; `call` boş `Bundle`; diğer metodlar kendi şekillerinde.
   Transaction kodları `--dump-class android.content.IContentProvider` ile
   okundu (QUERY=1, CALL=21, …).
6. **`DisplayInfo.supportedModes`** dolduruldu (`Display.Mode` ctor'u
   `(IIIF[F)V`); yoksa `Choreographer` "Unable to locate mode 1" atıyordu.
7. **`IPackageManager.getActivityInfo`** cevaplanıyor ve `ActivityInfo.theme`
   manifestteki `android:theme`'den geliyor; yoksa AppCompat
   "You need to use a Theme.AppCompat theme" atıyordu.
8. **SurfaceFlinger `createDisplayEventConnection`** (code 4, **statü kelimesi
   yok**) + `IDisplayEventConnection.stealReceiveChannel` (code 1,
   `[status][present=1][fd][fd]`). `BitTube` iki fd'dir; `Parcel::readData
   (Parcelable*)` önce `kNonNullParcelableFlag=1` okur.
9. **`IWindowManager.openSession` + `IWindowSession.addToDisplayAsUser`**.
   Reply shim'de kuruluyor çünkü `InputChannel` bir fd taşıyor ve
   `Parcel.marshall()` fd ifade edemiyor. `InputChannel`'ın tel formatı
   `[token binder][UTF-16 ad][fd]` (Android 13 `InputChannel::writeToParcel`).
   `activeControls` **tam olarak `InsetsState.SIZE` (26)** eleman olmalı:
   proxy `readTypedArray(T[], Creator)` kullanıyor ve uzunluk eşleşmezse
   "bad array lengths" atıyor.

### Vsync (bu turda eklendi)

`requestNextVsync` **oneway** bir çağrı; broker oneway işlemlerde `make_reply`'i
hiç çalıştırmıyordu, yani yan etki kayboluyordu. Artık oneway çağrılar da
işleniyor (cevap parcel'i atılıyor). `display_event_connection` code 3'te
`BitTube`'un send ucuna bir `DisplayEventReceiver::Event` yazılıyor
(216 bayt: `Header{type,displayId,timestamp}` + `VSync{count,VsyncEventData}`,
`frameInterval` 16.67 ms). Böylece `Choreographer` ilk kareyi alıyor.

### Grafik: hwui artık başlatılıyor (bu tur)

`relayout` gerçek bir `SurfaceControl` döndürüyor ve hwui EGL'ye kadar geliyor.
Zincir:

```
relayout → SurfaceControl parcel
  [String8 name][i32 w][i32 h][i32 hasNative=1]
  [client binder][handle binder][i32 layerId][u32 transformHint][u32 w][u32 h][u32 format]
→ SurfaceControl::readFromParcel (SurfaceControl.cpp:226)
→ Surface.copyFrom → getSurface() → generateSurfaceLocked()
    mClient->createSurface("bbq-wrapper", ...)   ← ISurfaceComposerClient code 1
    BLASTBufferQueue::make(...) → new BBQSurface(mProducer, ...)
→ hwui: eglCreateWindowSurface → gleshim (offscreen FBO) → eglSwapBuffers → daemon → Wayland
```

Bu turda eklenenler:

* `ISurfaceComposer.createConnection` (code 2) → `surface_composer_client`
* `ISurfaceComposerClient.createSurface` (code 1) →
  `[handle][gbp][layerId][transformHint][status]` — `callRemote` çıktı
  parametrelerini **önce**, statüyü **sonra** okur
* `relayout`'un `outSurfaceControl`'u artık dolu
* `ActivityInfo.flags = 0x200` (`FLAG_HARDWARE_ACCELERATED`) — `Activity.attach`
  bunu `Window.setWindowManager`'a geçiriyor; onsuz `ViewRootImpl` hiç
  `ThreadedRenderer` yaratmıyor
* `DisplayInfo.logicalDensityDpi = 420` — sıfırken `DisplayMetrics.density` 0
  oluyor, `ThreadedRenderer.setLightCenter`'ın `zRatio`'su Infinity'ye dönüşüyor
  ve ilk traversal "lightZ must be a finite positive" ile ölüyor
* `pengudroid-gleshim`: `eglQueryString(EGL_EXTENSIONS)` artık
  `EGL_KHR_swap_buffers_with_damage` bildiriyor ve
  `eglSwapBuffersWithDamageKHR` gerçekten var (hwui bu uzantı olmadan abort ediyor)
* `art-test` artık gleshim'i de `LD_PRELOAD` ediyor

**Kalan durak:** `EglManager::initialize` içinde
`Failed to acquire physical displays for WCG support!` — `getPhysicalDisplayIds`
çağrısı boş dönüyor. Eski (hand-written) etiket 34, ama AIDL tarafı kendi
numaralandırmasını kullanıyor; trace'te 2, 4, 8, 54 görülüyor. Sıradaki adım
doğru transaction kodunu bulup `[count][int64...]` döndürmek.

### NewPipe: ÇALIŞIYOR (bu tur)

`pengudroid art-test ... --classpath NewPipe_v0.29.1.apk` artık NewPipe'ı
**normal bir Linux süreci olarak** ayağa kaldırıyor ve 30 sn boyunca canlı
tutuyor (exit 124). Zincir:

```
handleBindApplication → handleLaunchActivity → handleResumeActivity
→ Looper.loop() → Choreographer → relayout (code 5) → OkHttp/HTTP2 → www.youtube.com
```

Bu turda düzeltilenler:

* **`ILocaleManager`.** `AppCompatDelegate.getApplicationLocales()` API 33+'ta
  locale servisine gider; cevap boş gelince `LocaleListCompat.wrap(null)`
  oluyor ve NewPipe'ın `Localization.getAppLocale()` içindeki `get(0)` NPE
  atıyordu. Broker artık code 2/3'e boş bir `LocaleList` döndürüyor.
* **`IWindowSession.relayout`** (code 5) cevaplanıyor: `[result][outFrames
  absent][mergedConfig absent][surfaceControl absent][insetsState absent]
  [activeControls = InsetsState.SIZE][null Bundle]`.
* **Vsync olay tipi.** Android 13 `DISPLAY_EVENT_VSYNC`'i `fourcc('v','s','y','n')`
  olarak yazıyor ve `fourcc` **big-endian** paketliyor: `0x7673796e`. Değer
  `libgui`'nin kendi `cmp`'inden okundu. (Yanlış değerle istemci her pulse'ta
  "ignoring unknown event type" diyordu.)
* **BitTube fd'leri.** Broker, istemciye verdiği fd'lerin *aynısını* tutuyordu;
  istemci `close()` edince bizim send ucumuz de kapanıyor ve vsync'ler
  düşüyordu (`dropped=1`). Artık `dup()` ile ayrı bir kopya tutuluyor —
  gerçek binder sürücüsünün yaptığı dup'ın elle hâli.
* **`statvfs`/`statfs`** (yukarıda), **`ResourcesManager.applyConfigurationToResources`**
  doğru imzayla, **`Configuration.userSetLocale`**.

Kalan tek gürültü: `Bundle: Key value expected String but value was a
Parcel$LazyValue` — `EMPTY_BUNDLE`'daki boş `HashMap` yüzünden, zararsız.

### (eski) NewPipe: layout inflate ediliyor, Activity onCreate'de duruyor

Bu turda NewPipe için düzeltilenler:

* **`setpriority` reddi.** Android uygulama süreçlerine `RLIMIT_NICE=40` verir;
  masaüstünde `(0,0)` olduğu için `Process.setThreadPriority` `EACCES` alıp
  `SecurityException` atıyor ve `HandlerThread.run` ölüyordu. Shim artık
  reddedilen nice değişikliğini kabul ediyor (yalnızca bir ipucu).
* **`IContentProvider` çift statü kelimesi.** `make_reply` zaten statüyü
  yazıyordu, `provider_reply` bir tane daha yazıyordu; `readBundle` fazladan
  sıfırı uzunluk sanıp boş `Bundle` döndürüyordu. Bu yüzden
  `Settings.NameValueCache.getStringsForPrefix` null map üzerinde NPE atıyordu.
* **ContentProvider kurulumu.** `AppBindData.providers` boştu, yani
  `androidx.startup.InitializationProvider` hiç çalışmıyordu ve WorkManager
  "not initialized properly" diyordu. Manifest'in `<provider>` girdileri artık
  `ProviderInfo` olarak geçiliyor (`grantUriPermissions` ve `<meta-data>` dahil).
* **`IPackageManager.getProviderInfo`** (code 15) cevaplanıyor; istek parceli
  sınıf adıyla eşleştiriliyor (parcel `[token][ComponentName][flags][userId]`
  olduğu için "son String16" yaklaşımı çalışmıyor).
* **`PackageInfo.signingInfo`** artık boş bir `SigningInfo` (ACRA ve Play
  Integrity null kontrolü yapmıyor).
* **`statvfs`/`statfs`** shim'de interpose edildi: `StatFs("/data")`
  `SQLiteGlobal.getDefaultPageSize` içinde her veritabanı açılışında çağrılıyor
  ve yol çevrilmediği için "Invalid path: /data" atıyordu.
* **`ResourcesManager.applyConfigurationToResources`** doğru imzayla
  (`getInstance()` + instance metodu, `Z` dönüşü) çağrılıyor.

Kalan durak: `MainActivity.onCreate` içinde
`LocaleListPlatformWrapper.get(0)` → `mLocaleList` null. Activity'nin kendi
`Resources`'u `ResourcesManager`'dan geliyor ve o `Configuration`'ın locale
listesi hâlâ null. Sıradaki adım: `ActivityClientRecord.overrideConfig`'in
gerçekten kullanıldığını doğrulamak ya da `Configuration.mSeq`'i artırıp
`applyConfigurationToResources`'ın config'i gerçekten yaymasını sağlamak.

### Roblox: şu anki tam durak noktası (SIGSEGV)

```
rbx.JNIRobloxSettings: Setting default app policy file: content/guac/...GlobalDist.json
→ SIGSEGV, libroblox.so + 0x224e4af
```

Çağrı zinciri (elle söküldü):

```
AppConfiguration::updateConfigurationDefaults(type, default)   @ 0x224e3c6
  log "[FLog::AppConfiguration] ... type = %u, default = %s"    (seviye yüzünden atlanıyor)
  singleton getter                                             @ 0x224242a  (std::call_once, null değil)
  lookup(r15, key)                                             @ 0x224e632
      → bulunamayınca çıktıya 16 sıfır yazıyor (0x224eaee)
  mov (%rax),%rcx        ← rax = null, çökme
```

Yani **AppConfiguration haritası boş**: `updateConfigurationDefaults` aradığı anahtarı
bulamıyor. `getStreamFromPath` bir *telemetri anahtarı* (map key), dosya yolu değil —
assets'i `app_assets/` ve `<data>/content/` altına elle çıkarmak çökmeyi
değiştirmiyor, yani sorun dosya erişimi değil.

Sıradaki adım: `0x224e632`'nin hangi anahtarı aradığını ve `AppConfiguration`'ın
nereden dolduğunu bulmak. `-Xcheck:jni` temiz geçiyor, yani JNI kötüye
kullanımı değil.

### Sıradaki adım (grafik)

`IWindowSession.relayout` (code 5) gerçek bir `SurfaceControl` döndürmeli.
Gerekenler:
- `ISurfaceComposer.createConnection` (code 2) → `ISurfaceComposerClient`
- `ISurfaceComposerClient.createSurface` → `SurfaceControl` (Java tarafı
  `SurfaceControl.writeToParcel` bir binder + handle yazar)
- `IGraphicBufferProducer` (`dequeueBuffer`/`queueBuffer`/`requestBuffer`) —
  broker tarafında bir ring buffer
- `pengudroid-hostgpu` + daemon'ın `zwp_linux_dmabuf_v1`'i ile Wayland'a sunum

### Teşhis araçları (bu oturumda eklendi)

- `tools/art/findclass_bp.c`: artık `BP_LIB` (hangi kütüphane), `BP_CALLER`
  (dönüş adresi + modül/offset), `BP_DUMP=<reg>:<n>` (bellek dökümü),
  `BP_STACKMAP` (stack'teki adresleri modüle çöz), `BP_REG` (r8/r9/rsp dahil).
- `PENGUDROID_TRACE_ROOTFS=1`: her yol çevirisini yazar.
- `--dump-class` artık **statik alanların değerini** de basıyor
  (`setAccessible` + `Field.get`), yani AIDL `*_TRANSACTION` sabitleri
  doğrudan okunabiliyor. Yeni `JniEnv` yardımcıları: `call_int`,
  `static_field_id`, `new_byte_array`, `set_byte_array_region`,
  `new_float_array`, `set_float_array_region`.

### Yeniden üretme

```sh
export PATH="$HOME/.cargo/bin:$PATH"
rm -rf /tmp/pengudroid-libs
timeout 120 env PENGUDROID_LAUNCH_ACTIVITY=com.roblox.client.startup.ActivitySplash \
  target/release/pengudroid art-test --sysroot .toolchain/sysroot \
  --register-natives libandroid_runtime.so,libhwui.so \
  --classpath /tmp/roblox/base.apk \
  --classpath /tmp/roblox/split_config.x86_64.apk \
  --bind-app --main android.app.ActivityThread --vm-option -Xint
```

Testler: `cargo test --workspace` → **174 geçiyor**.

---

için yazıldı. Ayrıntılı teknik gerekçeler `docs/LIMITATIONS.md` §2.1–2.4.

## Önceki oturumlar

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

Uygulama bağlandı ve **Activity örneği oluşturuluyor**:
`PENGUDROID_LAUNCH_ACTIVITY=<sınıf>` ile `ActivityThread.handleLaunchActivity`
doğrudan çağrılıyor (parcel/binder turu atlanıyor). Yol boyunca çözülenler:

- **`LoadedApk` sınıf yükleyicisi boş dex yoluyla geliyordu**
  (`DexPathList[[]]`, `mIncludeCode = 0`). `handleBindApplication`'ın
  `getPackageInfoNoCheck(..., includeCode=true)` çağrısı bu sürümde de false
  sonuç veriyor (taze bir çağrıda da `mIncludeCode = 0`,
  `mRegisterPackage`/`mSecurityViolation` ise beklendiği gibi false — yani
  argüman sırası kayması değil). Çözüm: bind sonrası `LoadedApk.mClassLoader`
  ve `mDefaultClassLoader` APK yoluna sahip bir `PathClassLoader` ile
  değiştiriliyor; `getClassLoader()` bunu döndürdüğü için Activity sınıfı
  çözülüyor (`app class loader resolved ...HTMLViewerActivity`).
- **`ActivityClient.getDisplayId`** → `IActivityTaskManager.getActivityClientController`
  (transaction **kodu 18**) boş yanıt alınca singleton null kalıyor ve
  `createBaseContextForActivity` NPE veriyordu. Broker artık bu çağrıya
  `activity_client_controller` handle'ı döndürüyor.
- **`Looper`**: `Activity`'nin kurucusu `Handler` oluşturuyor; bind/launch
  thread'inde Looper yoksa "Can't create handler inside thread ... that has not
  called Looper.prepare()". Launch öncesi `Looper.prepare()` çağrılıyor.

Şu an durduğu yer: **Activity başarıyla başlatılıyor.**

```
apphost: calling handleLaunchActivity for com.android.simappdialog.InstallCarrierAppActivity
apphost: handleLaunchActivity returned an Activity
```

Buradaki kilit hata **oneway transaction**'lardı: libbinder `TF_ONE_WAY`
çağrılarda bile `waitForResponse(nullptr, nullptr)` ile
**`BR_TRANSACTION_COMPLETE`** bekliyor (ama `BR_REPLY` beklemiyor). Broker
oneway'e hiçbir şey üretmediği için çağıran `talkWithDriver` içinde sonsuz
okuma döngüsüne giriyordu (ana thread `hrtimer_nanosleep`'te, %1.7 CPU).
Düzeltme: oneway için yalnızca `BR_TRANSACTION_COMPLETE` üretiliyor.

Buraya gelmek için ayrıca **display** taklit edildi:
`ContextImpl.getDisplayNoVerify()` → `ResourcesManager.getAdjustedDisplay` →
`DisplayManagerGlobal.getCompatibleDisplay` → `IDisplayManager.getDisplayInfo`
(transaction **kodu 1**). Broker, framework'ün kendi `DisplayInfo.writeToParcel`'ı
ile üretilmiş bir `DisplayInfo` döndürüyor (1280×720, `STATE_ON`, `TYPE_INTERNAL`,
`ROTATION_0`). Not: bu sürümde `DisplayInfo.densityDpi` alanı **yok**.

Test APK'sı `SimAppDialog` (`HTMLViewer` bir `WebView` istiyor, WebView
sağlayıcısı yok).

Sırada:
1. **Pencere / SurfaceFlinger** — şu anki duvar. `handleResumeActivity` çağrılıyor ve
   `onResume` sonrası `WindowManager.addView` yoluna giriyor:
   ```
   [window] code=4                      addToDisplayAsUser
   [SurfaceFlinger] code=2, code=4      createConnection / createDisplayEventConnection
   java.lang.RuntimeException: Failed to initialize display event receiver.  status=-19
   ```
   `Choreographer` → `DisplayEventReceiver.nativeInit` → `SurfaceFlinger`
   `createDisplayEventConnection` boş yanıt alıyor, `DisplayEventDispatcher`
   `ENODEV` dönüyor ve `DisplayEventReceiver` kurucusu istisna atıyor.

   **Ölçülen ayrıntılar (bu tur):**
   - `ISurfaceComposer.createConnection` (kod 2) ve `createDisplayEventConnection`
     (kod 4) **native AIDL**'dir ve yanıtlarında **durum sözcüğü yoktur**:
     okuyucu doğrudan offset 0'dan `readNullableStrongBinder` yapıyor (durum
     sözcüğü eklenince libbinder "Attempt to read object from Parcel ... at
     offset 0 that is not in the object list" diyor).
   - `DisplayEventConnection.stealReceiveChannel` (kod 1) çağrılıyor. İstemci
     tarafı (libgui disassembly'si) sırayla **`Status::readFromParcel`** ve
     **`Parcel::readParcelable`** → `BitTube::readFromParcel` çağırıyor.
     `Status::readFromParcel` OK için **4 bayt** okuyor (libbinder'da `je` ile
     doğrulandı: `readInt32` + `test` + `je`). `BitTube::writeToParcel` **iki fd**
     yazıyor (receive, send), `BINDER_TYPE_FD` (`0x66642a85`).
     - başlık yok → `PERMISSION_DENIED` (`readFileDescriptor` → `BAD_TYPE` = -1)
     - 4 bayt başlık + fd'ler [4, 28] → `PERMISSION_DENIED`
     - 8/12 bayt başlık → `UNEXPECTED_NULL`
     Yani başlık 4 bayt olmalı, ama fd nesneleri offset 4'te bulunamıyor.
     Sıradaki adım: `Parcel::readObject`'in `findObject` yolunu (0x77ff0) ve
     `mObjects`/`mObjectsSize`'ın `ipcSetDataReference`'tan doğru geldiğini
     doğrulamak. Deneme çalışmadığı için kod geri alındı.

   Gereken: `ISurfaceComposerClient.createSurface` + `IGraphicBufferProducer`
   (`connect`/`dequeueBuffer`/`queueBuffer`/`requestBuffer`/`setBufferCount`/
   `query`) ve bu buffer'ları **kendi EGL/dmabuf sunumumuza** bağlamak.
2. `IApplicationThread.scheduleLaunchActivity` (ActivityClientRecord parcel'ı)
   — şu an JNI ile doğrudan çağırıyoruz.
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
