/*
 * A NativeActivity app that exercises Vulkan: it loads the loader at runtime
 * (as an NDK app would through `libvulkan.so`), creates an instance, enumerates
 * physical devices, prints the GPU, creates a logical device with a graphics
 * queue, and exits.
 *
 * This proves that an Android app's Vulkan calls reach the *host* driver
 * (NVIDIA/AMD/Intel) through PenguDroid's soname mapping, with no translation
 * layer: Vulkan's ABI is identical on Android and Linux.
 *
 * Build: ./build.sh   (produces libmain.so)
 * Run:   pengudroid native --lib libmain.so
 */

#include <dlfcn.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

typedef struct ANativeActivity ANativeActivity;
typedef struct ANativeWindow ANativeWindow;
typedef struct AAssetManager AAssetManager;
typedef struct ARect { int32_t left, top, right, bottom; } ARect;

typedef struct ANativeActivityCallbacks {
    void (*onStart)(ANativeActivity*);
    void (*onResume)(ANativeActivity*);
    void* (*onSaveInstanceState)(ANativeActivity*, size_t*);
    void (*onPause)(ANativeActivity*);
    void (*onStop)(ANativeActivity*);
    void (*onDestroy)(ANativeActivity*);
    void (*onWindowFocusChanged)(ANativeActivity*, int);
    void (*onNativeWindowCreated)(ANativeActivity*, ANativeWindow*);
    void (*onNativeWindowResized)(ANativeActivity*, ANativeWindow*);
    void (*onNativeWindowRedrawNeeded)(ANativeActivity*, ANativeWindow*);
    void (*onNativeWindowDestroyed)(ANativeActivity*, ANativeWindow*);
    void (*onInputQueueCreated)(ANativeActivity*, void*);
    void (*onInputQueueDestroyed)(ANativeActivity*, void*);
    void (*onContentRectChanged)(ANativeActivity*, const ARect*);
    void (*onConfigurationChanged)(ANativeActivity*);
    void (*onLowMemory)(ANativeActivity*);
} ANativeActivityCallbacks;

struct ANativeActivity {
    ANativeActivityCallbacks* callbacks;
    void* vm;
    void* env;
    void* clazz;
    const char* internalDataPath;
    const char* externalDataPath;
    int32_t sdkVersion;
    void* instance;
    AAssetManager* assetManager;
    const char* obbPath;
};

extern void ANativeActivity_finish(ANativeActivity*);

/* ------------------------------------------------------------------ */
/* Minimal Vulkan declarations (no headers on the host)                */
/* ------------------------------------------------------------------ */

typedef void* VkInstance;
typedef void* VkPhysicalDevice;
typedef void* VkDevice;
typedef void* VkQueue;
typedef int VkResult;

#define VK_SUCCESS 0
#define VK_STRUCTURE_TYPE_APPLICATION_INFO 0
#define VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO 1
#define VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO 3
#define VK_QUEUE_GRAPHICS_BIT 0x00000001u

typedef struct VkApplicationInfo {
    int32_t sType;
    const void* pNext;
    const char* pApplicationName;
    uint32_t applicationVersion;
    const char* pEngineName;
    uint32_t engineVersion;
    uint32_t apiVersion;
} VkApplicationInfo;

typedef struct VkInstanceCreateInfo {
    int32_t sType;
    const void* pNext;
    uint32_t flags;
    const VkApplicationInfo* pApplicationInfo;
    uint32_t enabledLayerCount;
    const char* const* ppEnabledLayerNames;
    uint32_t enabledExtensionCount;
    const char* const* ppEnabledExtensionNames;
} VkInstanceCreateInfo;

typedef struct VkQueueFamilyProperties {
    uint32_t queueFlags;
    uint32_t queueCount;
    uint32_t timestampValidBits;
    uint32_t minImageTransferGranularity[3];
} VkQueueFamilyProperties;

typedef struct VkDeviceQueueCreateInfo {
    int32_t sType;
    const void* pNext;
    uint32_t flags;
    uint32_t queueFamilyIndex;
    uint32_t queueCount;
    const float* pQueuePriorities;
} VkDeviceQueueCreateInfo;

typedef struct VkDeviceCreateInfo {
    int32_t sType;
    const void* pNext;
    uint32_t flags;
    uint32_t queueCreateInfoCount;
    const VkDeviceQueueCreateInfo* pQueueCreateInfos;
    uint32_t enabledLayerCount;
    const char* const* ppEnabledLayerNames;
    uint32_t enabledExtensionCount;
    const char* const* ppEnabledExtensionNames;
    const void* pEnabledFeatures;
} VkDeviceCreateInfo;

typedef VkResult (*PFN_vkCreateInstance)(const VkInstanceCreateInfo*, const void*, VkInstance*);
typedef VkResult (*PFN_vkEnumeratePhysicalDevices)(VkInstance, uint32_t*, VkPhysicalDevice*);
typedef void (*PFN_vkGetPhysicalDeviceProperties)(VkPhysicalDevice, void*);
typedef void (*PFN_vkGetPhysicalDeviceQueueFamilyProperties)(
    VkPhysicalDevice, uint32_t*, VkQueueFamilyProperties*);
typedef VkResult (*PFN_vkCreateDevice)(
    VkPhysicalDevice, const VkDeviceCreateInfo*, const void*, VkDevice*);
typedef void (*PFN_vkGetDeviceQueue)(VkDevice, uint32_t, uint32_t, VkQueue*);
typedef void (*PFN_vkDestroyDevice)(VkDevice, const void*);
typedef void (*PFN_vkDestroyInstance)(VkInstance, const void*);

/* VkPhysicalDeviceProperties is large; only the first fields are read here. */
typedef struct {
    uint32_t apiVersion;
    uint32_t driverVersion;
    uint32_t vendorID;
    uint32_t deviceID;
    uint32_t deviceType;
    char deviceName[256];
} DevicePropertiesHead;

static void* g_lib;

static void* vk_sym(const char* name) {
    void* p = dlsym(g_lib, name);
    if (!p) {
        fprintf(stderr, "native-vulkan: missing %s\n", name);
    }
    return p;
}

void ANativeActivity_onCreate(ANativeActivity* activity, void* saved_state, size_t saved_size) {
    (void)saved_state;
    (void)saved_size;

    g_lib = dlopen("libvulkan.so", RTLD_NOW | RTLD_LOCAL);
    if (!g_lib) {
        g_lib = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
    }
    if (!g_lib) {
        fprintf(stderr, "native-vulkan: cannot load libvulkan: %s\n", dlerror());
        ANativeActivity_finish(activity);
        return;
    }

    PFN_vkCreateInstance create_instance = (PFN_vkCreateInstance)vk_sym("vkCreateInstance");
    PFN_vkEnumeratePhysicalDevices enumerate =
        (PFN_vkEnumeratePhysicalDevices)vk_sym("vkEnumeratePhysicalDevices");
    PFN_vkGetPhysicalDeviceProperties get_props =
        (PFN_vkGetPhysicalDeviceProperties)vk_sym("vkGetPhysicalDeviceProperties");
    PFN_vkGetPhysicalDeviceQueueFamilyProperties get_queues =
        (PFN_vkGetPhysicalDeviceQueueFamilyProperties)vk_sym(
            "vkGetPhysicalDeviceQueueFamilyProperties");
    PFN_vkCreateDevice create_device = (PFN_vkCreateDevice)vk_sym("vkCreateDevice");
    PFN_vkGetDeviceQueue get_queue = (PFN_vkGetDeviceQueue)vk_sym("vkGetDeviceQueue");
    PFN_vkDestroyDevice destroy_device = (PFN_vkDestroyDevice)vk_sym("vkDestroyDevice");
    PFN_vkDestroyInstance destroy_instance =
        (PFN_vkDestroyInstance)vk_sym("vkDestroyInstance");
    if (!create_instance || !enumerate || !get_props || !get_queues || !create_device ||
        !get_queue || !destroy_device || !destroy_instance) {
        ANativeActivity_finish(activity);
        return;
    }

    VkApplicationInfo app = {0};
    app.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
    app.pApplicationName = "PenguDroid Vulkan test";
    app.applicationVersion = 1;
    app.pEngineName = "PenguDroid";
    app.engineVersion = 1;
    app.apiVersion = (1u << 22) | (1u << 12); /* 1.1 */

    VkInstanceCreateInfo ci = {0};
    ci.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
    ci.pApplicationInfo = &app;

    VkInstance instance = NULL;
    VkResult r = create_instance(&ci, NULL, &instance);
    if (r != VK_SUCCESS || !instance) {
        fprintf(stderr, "native-vulkan: vkCreateInstance failed (%d)\n", r);
        ANativeActivity_finish(activity);
        return;
    }

    uint32_t count = 0;
    r = enumerate(instance, &count, NULL);
    if (r != VK_SUCCESS || count == 0) {
        fprintf(stderr, "native-vulkan: no physical devices (%d)\n", r);
        destroy_instance(instance, NULL);
        ANativeActivity_finish(activity);
        return;
    }
    VkPhysicalDevice devices[8];
    if (count > 8) count = 8;
    r = enumerate(instance, &count, devices);
    if (r != VK_SUCCESS) {
        fprintf(stderr, "native-vulkan: vkEnumeratePhysicalDevices failed (%d)\n", r);
        destroy_instance(instance, NULL);
        ANativeActivity_finish(activity);
        return;
    }

    /* Read the head of VkPhysicalDeviceProperties (deviceName is at offset 20). */
    unsigned char props[4096];
    memset(props, 0, sizeof(props));
    get_props(devices[0], props);
    DevicePropertiesHead* head = (DevicePropertiesHead*)props;
    fprintf(stderr,
            "native-vulkan: device 0: %s (api %u.%u.%u, vendor 0x%04x)\n",
            head->deviceName,
            (head->apiVersion >> 22) & 0x7f,
            (head->apiVersion >> 12) & 0x3ff,
            head->apiVersion & 0xfff,
            head->vendorID);

    uint32_t qcount = 0;
    get_queues(devices[0], &qcount, NULL);
    if (qcount == 0) {
        fprintf(stderr, "native-vulkan: no queue families\n");
        destroy_instance(instance, NULL);
        ANativeActivity_finish(activity);
        return;
    }
    VkQueueFamilyProperties* queues = (VkQueueFamilyProperties*)props;
    memset(props, 0, sizeof(props));
    get_queues(devices[0], &qcount, queues);
    uint32_t graphics = UINT32_MAX;
    for (uint32_t i = 0; i < qcount && i < 16; i++) {
        if (queues[i].queueFlags & VK_QUEUE_GRAPHICS_BIT) {
            graphics = i;
            break;
        }
    }
    if (graphics == UINT32_MAX) {
        fprintf(stderr, "native-vulkan: no graphics queue family\n");
        destroy_instance(instance, NULL);
        ANativeActivity_finish(activity);
        return;
    }

    float priority = 1.0f;
    VkDeviceQueueCreateInfo qci = {0};
    qci.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
    qci.queueFamilyIndex = graphics;
    qci.queueCount = 1;
    qci.pQueuePriorities = &priority;

    VkDeviceCreateInfo dci = {0};
    dci.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
    dci.queueCreateInfoCount = 1;
    dci.pQueueCreateInfos = &qci;

    VkDevice device = NULL;
    r = create_device(devices[0], &dci, NULL, &device);
    if (r != VK_SUCCESS || !device) {
        fprintf(stderr, "native-vulkan: vkCreateDevice failed (%d)\n", r);
        destroy_instance(instance, NULL);
        ANativeActivity_finish(activity);
        return;
    }

    VkQueue queue = NULL;
    get_queue(device, graphics, 0, &queue);
    fprintf(stderr, "native-vulkan: logical device + graphics queue created (%s)\n",
            queue ? "ok" : "no queue");

    destroy_device(device, NULL);
    destroy_instance(instance, NULL);
    fprintf(stderr, "native-vulkan: done\n");
    ANativeActivity_finish(activity);
}
