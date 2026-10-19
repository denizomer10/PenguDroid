# `native-vulkan` fixture

A NativeActivity app that loads `libvulkan.so` at runtime, creates an instance,
enumerates physical devices, prints the GPU, and creates a logical device with a
graphics queue.

```sh
./build.sh
pengudroid native --lib libmain.so
```

On an RTX 3080 it prints:

```
native-vulkan: device 0: NVIDIA GeForce RTX 3080 (api 1.4.329, vendor 0x10de)
native-vulkan: logical device + graphics queue created (ok)
```

This works because `pengudroid libpath` maps the Bionic soname `libvulkan.so`
onto the host loader, and Vulkan's ABI is identical on Android and Linux.
