# API dokümantasyonu

Bu dosya, PenguDroid'in **kontrol protokolünü** ve CLI yüzeyini kullanıcı
açısından tek yerde toplar. Uygulamanın neyi çalıştırdığı/neyi çalıştırmadığı
`docs/LIMITATIONS.md`, katmanların nasıl çalıştığı `docs/ARCHITECTURE.md`'de.

## Komut satırı

| Komut | Ne yapar | Durum |
|---|---|---|
| `pengudroid daemon [--foreground]` | daemon'ı başlatır (zaten çalışıyorsa sessizce çıkar) | çalışır |
| `pengudroid status` | daemon'a ping atar, guest/servis/property sayılarını yazar | çalışır |
| `pengudroid stop` | daemon'ı kapatır ve soketin yok olmasını bekler | çalışır |
| `pengudroid demo` | dahili demo penceresi (APK gerekmez) | çalışır (Wayland oturumu gerekir) |
| `pengudroid native <apk>` / `--lib lib.so` | NativeActivity uygulamasını ART olmadan çalıştırır; `--isolate` seccomp ekler | çalışır |
| `pengudroid launch <app.apk>` | Proton tarzı başlatıcı: uygulama başına prefix (`~/.local/share/pengudroid/prefixes/<pkg>`), `pengudroid.env` ile ortam geçersiz kılma, arka ucu (`native`/`art`) APK'dan seçer; `--backend`, `--prefix`, `--sysroot` | çalışır |
| `pengudroid extract <file> [--dest DIR] [--abi A]` | `.apk`, `.apkm`, `.xapk`, `.apks` paketlerini açar: base APK + split'ler (ABI filtresi ile) + OBB verisi `Android/obb/<pkg>/` düzeninde | çalışır |
| `pengudroid doctor <apk>` | APK çalışabilir mi, ne eksik? | çalışır |
| `pengudroid sysroot <dir>` | çıkarılmış Android sistem ağacını denetler | çalışır |
| `pengudroid art-test --sysroot <dir>` | `JNI_CreateJavaVM`'i dener; ek olarak `--classpath <apk>`, `--find-class <C>`, `--main <C>`, `--jni-lib <so>`, `--register-natives <so>` | ÇALIŞIR — canlı `JavaVM` (`JNI_OK`) |
| `pengudroid run app.apk` | guest'i CLI altında başlatır, Ctrl-C ile birlikte durur; olmayan APK hata olur | çalışır (Wayland oturumu gerekir) |
| `pengudroid start app.apk [--wait]` | guest'i *daemon* altında başlatır; `--wait` çıkış kodunu bekler | çalışır |
| `pengudroid start --demo [--wait]` | aynısı, dahili demo ile | çalışır |
| `pengudroid ps` | çalışan guest'leri daemon'ın tablosundan listeler | çalışır |
| `pengudroid services` | broker'ın servis tablosunu (handle/isim/arayüz) yazar | çalışır |
| `pengudroid getprop [NAME]` | bir özelliği okur; NAME yoksa hepsini listeler | çalışır |
| `pengudroid setprop NAME VALUE` | özellik yazar (daemon kapanırken diske kaydedilir) | çalışır |
| `pengudroid info app.apk` | manifest alanlarını yazar | çalışır |
| `pengudroid deps lib.so` | Bionic import kapsama oranı | çalışır |
| `pengudroid nativelib …`, `libpath`, `de-bionic` | APK `.so` dosyalarını glibc üzerinde yükleme | çalışır |
| `pengudroid gpu-info`, `gpu-test` | host EGL/GLES yetenekleri, dmabuf dışa aktarımı | çalışır |
| `pengudroid-daemon --wayland-selftest` | Wayland bağlantısını ve dmabuf uzantılarını sınar | çalışır |
| `pengudroid-daemon --dmabuf-selftest` | GPU'da render edip dmabuf'ı compositor'a verir | çalışır |

## Protokol (v2)

Taşıma: `$XDG_RUNTIME_DIR/pengudroid/daemon.sock` (mode `0600`), uzunluk önekli
çerçeve `[u32 body_len][body]`, `body = [kind u8][alanlar…]`. Dosya tanıtıcıları
`SCM_RIGHTS` ile çerçeve dışında geçer. Her bağlantı `Hello`/`Welcome` ile
başlar; `Hello.role` = `Cli`, `Shim` veya `Guest`.

| Mesaj | Yön | Anlamı |
|---|---|---|
| `Ping` / `Pong` | her iki | canlılık |
| `RunApk { path, component?, args }` → `RunStarted { pid, package, surface_id }` | CLI → daemon | guest'i daemon fork eder |
| `QueryRun { pid }` → `RunStatus { pid, running, exit_code, signal }` | CLI → daemon | fork edilen guest'in durumu |
| `RegisterProc` / `UnregisterProc` / `ListProcs` → `ProcList` | guest/CLI | `ps` tablosu |
| `AllocShmem { name, size }` → `ShmemAllocd { name, size, seam }` + fd | guest → daemon | ashmem (memfd) |
| `RegisterService { name, iface }` → `ServiceRegistered { handle }` | guest → daemon | guest servisi yayınlama (platform adlarına izin yok) |
| `ListServices` → `ServiceList { (name, iface, handle)… }` | CLI → daemon | servis tablosu |
| `BinderTransact { handle, code, flags, data }` → `BinderReply { status, data }` | CLI/guest → daemon | stub'a ya da guest sahibine yönlendirilir |
| `GuestCall { code, data, token }` / `GuestCallReply { status, data }` | daemon ↔ guest | guest servisine iletim ve cevap |
| `CreateSurface { width, height, title }` → `SurfaceCreated { … }` + fd | guest → daemon | Wayland toplevel + paylaşılan buffer |
| `PresentFrame { surface_id }` → `Ack` | guest → daemon | buffer'ı commit et |
| `GetProp` / `SetProp` / `ListProps` | CLI/guest | özellik deposu |
| `Shutdown` → `Goodbye` | CLI → daemon | temiz kapanış |

Hata durumları `Error { code, message }` (`code` = negatif errno) ile döner:
`-2` dosya yok, `-3` bilinmeyen pid, `-5` spawn/fd hatası, `-17` ad çakışması,
`-19` grafik yok, `-22` bilinmeyen handle, `-32` guest bağlantısı düşmüş,
`-110` guest servisi zaman aşımı (5 s).

## Yerel doğrulama

Bu depoda derlemek için:

```sh
cargo build --workspace
cargo test --workspace
```

Testler daemon'un dispatch tablosunu gerçek bir `Daemon` üzerinden çağırır
(proses/soket açmadan), bu yüzden kısıtlı sandbox'larda da çalışır.
