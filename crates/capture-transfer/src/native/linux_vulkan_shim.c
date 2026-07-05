#include <dlfcn.h>
#include <errno.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define VK_SUCCESS 0
#define VK_INCOMPLETE 5
#define VK_STRUCTURE_TYPE_APPLICATION_INFO 0
#define VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO 1
#define VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO 2
#define VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO 3
#define VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO 5
#define VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO 14
#define VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2 1000059002
#define VK_STRUCTURE_TYPE_BIND_IMAGE_MEMORY_INFO 1000157001
#define VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS 1000127000
#define VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO 1000127001
#define VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2 1000146001
#define VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2 1000146003
#define VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO 1000072001
#define VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR 1000074000
#define VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR 1000074001
#define VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT 1000158004
#define VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT 1000158006
#define VK_API_VERSION_1_0 4194304u
#define VK_MAX_MEMORY_TYPES 32u
#define VK_MAX_MEMORY_HEAPS 16u
#define VK_FORMAT_R8G8B8A8_UNORM 37
#define VK_FORMAT_B8G8R8A8_UNORM 44
#define VK_IMAGE_TYPE_2D 1
#define VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT 1000158000
#define VK_IMAGE_USAGE_SAMPLED_BIT 0x00000004u
#define VK_SHARING_MODE_EXCLUSIVE 0
#define VK_IMAGE_LAYOUT_UNDEFINED 0
#define VK_SAMPLE_COUNT_1_BIT 0x00000001u
#define VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT 0x00000200u
#define VK_IMAGE_ASPECT_COLOR_BIT 0x00000001u
#define VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT 0x00000001u

typedef int32_t VkResult;
typedef uint32_t VkFlags;
typedef uint32_t VkStructureType;
typedef uint32_t VkBool32;
typedef uint64_t VkDeviceSize;
typedef uint32_t VkFormatFeatureFlags;
typedef void *VkInstance;
typedef void *VkPhysicalDevice;
typedef void *VkDevice;
typedef void *VkImage;
typedef void *VkDeviceMemory;

typedef uint32_t VkImageCreateFlags;
typedef uint32_t VkImageType;
typedef uint32_t VkFormat;
typedef uint32_t VkSampleCountFlagBits;
typedef uint32_t VkImageTiling;
typedef uint32_t VkImageUsageFlags;
typedef uint32_t VkSharingMode;
typedef uint32_t VkImageLayout;
typedef uint32_t VkExternalMemoryHandleTypeFlags;
typedef uint32_t VkExternalMemoryHandleTypeFlagBits;
typedef uint32_t VkImageAspectFlags;
typedef uint32_t VkMemoryPropertyFlags;
typedef uint32_t VkMemoryHeapFlags;

typedef struct VkExtensionProperties {
  char extensionName[256];
  uint32_t specVersion;
} VkExtensionProperties;

typedef struct VkFormatProperties {
  VkFormatFeatureFlags linearTilingFeatures;
  VkFormatFeatureFlags optimalTilingFeatures;
  VkFormatFeatureFlags bufferFeatures;
} VkFormatProperties;

typedef struct VkFormatProperties2 {
  VkStructureType sType;
  void *pNext;
  VkFormatProperties formatProperties;
} VkFormatProperties2;

typedef struct VkApplicationInfo {
  VkStructureType sType;
  const void *pNext;
  const char *pApplicationName;
  uint32_t applicationVersion;
  const char *pEngineName;
  uint32_t engineVersion;
  uint32_t apiVersion;
} VkApplicationInfo;

typedef struct VkInstanceCreateInfo {
  VkStructureType sType;
  const void *pNext;
  VkFlags flags;
  const VkApplicationInfo *pApplicationInfo;
  uint32_t enabledLayerCount;
  const char *const *ppEnabledLayerNames;
  uint32_t enabledExtensionCount;
  const char *const *ppEnabledExtensionNames;
} VkInstanceCreateInfo;

typedef struct VkExtent3D {
  uint32_t width;
  uint32_t height;
  uint32_t depth;
} VkExtent3D;

typedef struct VkMemoryType {
  VkMemoryPropertyFlags propertyFlags;
  uint32_t heapIndex;
} VkMemoryType;

typedef struct VkMemoryHeap {
  VkDeviceSize size;
  VkMemoryHeapFlags flags;
} VkMemoryHeap;

typedef struct VkPhysicalDeviceMemoryProperties {
  uint32_t memoryTypeCount;
  VkMemoryType memoryTypes[VK_MAX_MEMORY_TYPES];
  uint32_t memoryHeapCount;
  VkMemoryHeap memoryHeaps[VK_MAX_MEMORY_HEAPS];
} VkPhysicalDeviceMemoryProperties;

typedef struct VkQueueFamilyProperties {
  VkFlags queueFlags;
  uint32_t queueCount;
  uint32_t timestampValidBits;
  VkExtent3D minImageTransferGranularity;
} VkQueueFamilyProperties;

typedef struct VkDeviceQueueCreateInfo {
  VkStructureType sType;
  const void *pNext;
  VkFlags flags;
  uint32_t queueFamilyIndex;
  uint32_t queueCount;
  const float *pQueuePriorities;
} VkDeviceQueueCreateInfo;

typedef struct VkDeviceCreateInfo {
  VkStructureType sType;
  const void *pNext;
  VkFlags flags;
  uint32_t queueCreateInfoCount;
  const VkDeviceQueueCreateInfo *pQueueCreateInfos;
  uint32_t enabledLayerCount;
  const char *const *ppEnabledLayerNames;
  uint32_t enabledExtensionCount;
  const char *const *ppEnabledExtensionNames;
  const void *pEnabledFeatures;
} VkDeviceCreateInfo;

typedef struct VkSubresourceLayout {
  VkDeviceSize offset;
  VkDeviceSize size;
  VkDeviceSize rowPitch;
  VkDeviceSize arrayPitch;
  VkDeviceSize depthPitch;
} VkSubresourceLayout;

typedef struct VkExternalMemoryImageCreateInfo {
  VkStructureType sType;
  const void *pNext;
  VkExternalMemoryHandleTypeFlags handleTypes;
} VkExternalMemoryImageCreateInfo;

typedef struct VkImageDrmFormatModifierExplicitCreateInfoEXT {
  VkStructureType sType;
  const void *pNext;
  uint64_t drmFormatModifier;
  uint32_t drmFormatModifierPlaneCount;
  const VkSubresourceLayout *pPlaneLayouts;
} VkImageDrmFormatModifierExplicitCreateInfoEXT;

typedef struct VkDrmFormatModifierPropertiesEXT {
  uint64_t drmFormatModifier;
  uint32_t drmFormatModifierPlaneCount;
  VkFormatFeatureFlags drmFormatModifierTilingFeatures;
} VkDrmFormatModifierPropertiesEXT;

typedef struct VkDrmFormatModifierPropertiesListEXT {
  VkStructureType sType;
  void *pNext;
  uint32_t drmFormatModifierCount;
  VkDrmFormatModifierPropertiesEXT *pDrmFormatModifierProperties;
} VkDrmFormatModifierPropertiesListEXT;

typedef struct VkImageCreateInfo {
  VkStructureType sType;
  const void *pNext;
  VkImageCreateFlags flags;
  VkImageType imageType;
  VkFormat format;
  VkExtent3D extent;
  uint32_t mipLevels;
  uint32_t arrayLayers;
  VkSampleCountFlagBits samples;
  VkImageTiling tiling;
  VkImageUsageFlags usage;
  VkSharingMode sharingMode;
  uint32_t queueFamilyIndexCount;
  const uint32_t *pQueueFamilyIndices;
  VkImageLayout initialLayout;
} VkImageCreateInfo;

typedef struct VkMemoryRequirements {
  VkDeviceSize size;
  VkDeviceSize alignment;
  uint32_t memoryTypeBits;
} VkMemoryRequirements;

typedef struct VkMemoryDedicatedRequirements {
  VkStructureType sType;
  void *pNext;
  VkBool32 prefersDedicatedAllocation;
  VkBool32 requiresDedicatedAllocation;
} VkMemoryDedicatedRequirements;

typedef struct VkMemoryRequirements2 {
  VkStructureType sType;
  void *pNext;
  VkMemoryRequirements memoryRequirements;
} VkMemoryRequirements2;

typedef struct VkImageMemoryRequirementsInfo2 {
  VkStructureType sType;
  const void *pNext;
  VkImage image;
} VkImageMemoryRequirementsInfo2;

typedef struct VkMemoryFdPropertiesKHR {
  VkStructureType sType;
  void *pNext;
  uint32_t memoryTypeBits;
} VkMemoryFdPropertiesKHR;

typedef struct VkImportMemoryFdInfoKHR {
  VkStructureType sType;
  const void *pNext;
  VkExternalMemoryHandleTypeFlagBits handleType;
  int fd;
} VkImportMemoryFdInfoKHR;

typedef struct VkMemoryDedicatedAllocateInfo {
  VkStructureType sType;
  const void *pNext;
  VkImage image;
  void *buffer;
} VkMemoryDedicatedAllocateInfo;

typedef struct VkMemoryAllocateInfo {
  VkStructureType sType;
  const void *pNext;
  VkDeviceSize allocationSize;
  uint32_t memoryTypeIndex;
} VkMemoryAllocateInfo;

typedef struct VkBindImageMemoryInfo {
  VkStructureType sType;
  const void *pNext;
  VkImage image;
  VkDeviceMemory memory;
  VkDeviceSize memoryOffset;
} VkBindImageMemoryInfo;

typedef const void *(*PFN_vkGetInstanceProcAddr)(VkInstance instance, const char *name);
typedef const void *(*PFN_vkGetDeviceProcAddr)(VkDevice device, const char *name);
typedef VkResult (*PFN_vkEnumerateInstanceExtensionProperties)(const char *layer_name,
                                                               uint32_t *property_count,
                                                               VkExtensionProperties *properties);
typedef VkResult (*PFN_vkCreateInstance)(const VkInstanceCreateInfo *create_info,
                                         const void *allocator,
                                         VkInstance *instance);
typedef void (*PFN_vkDestroyInstance)(VkInstance instance, const void *allocator);
typedef VkResult (*PFN_vkEnumeratePhysicalDevices)(VkInstance instance,
                                                   uint32_t *physical_device_count,
                                                   VkPhysicalDevice *physical_devices);
typedef VkResult (*PFN_vkEnumerateDeviceExtensionProperties)(VkPhysicalDevice physical_device,
                                                             const char *layer_name,
                                                             uint32_t *property_count,
                                                             VkExtensionProperties *properties);
typedef void (*PFN_vkGetPhysicalDeviceFormatProperties2KHR)(VkPhysicalDevice physical_device,
                                                            VkFormat format,
                                                            VkFormatProperties2 *format_properties);
typedef void (*PFN_vkGetPhysicalDeviceQueueFamilyProperties)(VkPhysicalDevice physical_device,
                                                             uint32_t *queue_family_property_count,
                                                             VkQueueFamilyProperties *queue_family_properties);
typedef void (*PFN_vkGetPhysicalDeviceMemoryProperties)(VkPhysicalDevice physical_device,
                                                        VkPhysicalDeviceMemoryProperties *memory_properties);
typedef VkResult (*PFN_vkCreateDevice)(VkPhysicalDevice physical_device,
                                       const VkDeviceCreateInfo *create_info,
                                       const void *allocator,
                                       VkDevice *device);
typedef void (*PFN_vkDestroyDevice)(VkDevice device, const void *allocator);
typedef VkResult (*PFN_vkCreateImage)(VkDevice device, const VkImageCreateInfo *create_info, const void *allocator, VkImage *image);
typedef void (*PFN_vkDestroyImage)(VkDevice device, VkImage image, const void *allocator);
typedef void (*PFN_vkGetImageMemoryRequirements2KHR)(VkDevice device,
                                                     const VkImageMemoryRequirementsInfo2 *info,
                                                     VkMemoryRequirements2 *memory_requirements);
typedef VkResult (*PFN_vkGetMemoryFdPropertiesKHR)(VkDevice device,
                                                   VkExternalMemoryHandleTypeFlagBits handle_type,
                                                   int fd,
                                                   VkMemoryFdPropertiesKHR *memory_fd_properties);
typedef VkResult (*PFN_vkAllocateMemory)(VkDevice device,
                                         const VkMemoryAllocateInfo *allocate_info,
                                         const void *allocator,
                                         VkDeviceMemory *memory);
typedef void (*PFN_vkFreeMemory)(VkDevice device, VkDeviceMemory memory, const void *allocator);
typedef VkResult (*PFN_vkBindImageMemory2KHR)(VkDevice device,
                                              uint32_t bind_info_count,
                                              const VkBindImageMemoryInfo *bind_infos);

struct porthole_native_linux_vulkan_probe {
  uint32_t struct_size;
  uint32_t loader_present;
  uint32_t can_enumerate_instance_extensions;
  uint32_t can_create_instance;
  uint32_t physical_device_count;
  uint32_t instance_extension_count;
  uint32_t has_get_physical_device_properties2;
  uint32_t has_external_memory_capabilities;
  uint32_t has_external_semaphore_capabilities;
  uint32_t has_external_fence_capabilities;
  uint32_t has_external_memory_dma_buf;
  uint32_t has_external_memory_fd;
  uint32_t has_image_drm_format_modifier;
  uint32_t has_get_memory_requirements2;
  uint32_t has_bind_memory2;
  uint32_t has_queue_family_foreign;
  uint32_t has_external_semaphore_fd;
  uint32_t has_external_fence_fd;
  uint32_t has_timeline_semaphore;
  uint32_t can_create_reference_device;
  uint32_t has_image_import_device_functions;
};

struct porthole_native_linux_vulkan_import_plane {
  uint32_t offset;
  uint32_t stride;
};

struct porthole_native_linux_vulkan_import_image {
  uint32_t struct_size;
  int32_t fd;
  uint32_t width;
  uint32_t height;
  uint32_t vk_format;
  uint64_t modifier;
  uint32_t plane_count;
  struct porthole_native_linux_vulkan_import_plane planes[4];
};

struct porthole_native_linux_vulkan_modifier_query {
  uint32_t struct_size;
  uint32_t vk_format;
  uint32_t modifier_capacity;
  uint32_t modifier_count;
  uint64_t *modifiers;
};

struct porthole_native_linux_vk_reference_device {
  void *loader;
  PFN_vkGetInstanceProcAddr get_instance_proc;
  PFN_vkGetDeviceProcAddr get_device_proc;
  PFN_vkDestroyInstance destroy_instance;
  PFN_vkDestroyDevice destroy_device;
  PFN_vkCreateImage create_image;
  PFN_vkDestroyImage destroy_image;
  PFN_vkGetImageMemoryRequirements2KHR get_image_memory_requirements2;
  PFN_vkGetMemoryFdPropertiesKHR get_memory_fd_properties;
  PFN_vkAllocateMemory allocate_memory;
  PFN_vkFreeMemory free_memory;
  PFN_vkBindImageMemory2KHR bind_image_memory2;
  VkInstance instance;
  VkPhysicalDevice physical_device;
  VkDevice device;
  VkPhysicalDeviceMemoryProperties memory_properties;
};

static int porthole_native_linux_vk_result(VkResult result) {
  if (result == VK_SUCCESS || result == VK_INCOMPLETE) {
    return 0;
  }
  return EIO;
}

static int porthole_native_linux_vk_extension_is(const VkExtensionProperties *property, const char *name) {
  return strncmp(property->extensionName, name, sizeof(property->extensionName)) == 0;
}

static void porthole_native_linux_vk_record_instance_extension(
    const VkExtensionProperties *property,
    struct porthole_native_linux_vulkan_probe *out) {
  if (porthole_native_linux_vk_extension_is(property, "VK_KHR_get_physical_device_properties2")) {
    out->has_get_physical_device_properties2 = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_KHR_external_memory_capabilities")) {
    out->has_external_memory_capabilities = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_KHR_external_semaphore_capabilities")) {
    out->has_external_semaphore_capabilities = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_KHR_external_fence_capabilities")) {
    out->has_external_fence_capabilities = 1;
  }
}

static void porthole_native_linux_vk_record_device_extension(
    const VkExtensionProperties *property,
    struct porthole_native_linux_vulkan_probe *out) {
  if (porthole_native_linux_vk_extension_is(property, "VK_EXT_external_memory_dma_buf")) {
    out->has_external_memory_dma_buf = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_KHR_external_memory_fd")) {
    out->has_external_memory_fd = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_EXT_image_drm_format_modifier")) {
    out->has_image_drm_format_modifier = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_KHR_get_memory_requirements2")) {
    out->has_get_memory_requirements2 = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_KHR_bind_memory2")) {
    out->has_bind_memory2 = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_EXT_queue_family_foreign")) {
    out->has_queue_family_foreign = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_KHR_external_semaphore_fd")) {
    out->has_external_semaphore_fd = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_KHR_external_fence_fd")) {
    out->has_external_fence_fd = 1;
  } else if (porthole_native_linux_vk_extension_is(property, "VK_KHR_timeline_semaphore")) {
    out->has_timeline_semaphore = 1;
  }
}

static int porthole_native_linux_vk_has_reference_device_extensions(
    const struct porthole_native_linux_vulkan_probe *probe) {
  return probe->has_external_memory_dma_buf && probe->has_external_memory_fd &&
         probe->has_image_drm_format_modifier && probe->has_get_memory_requirements2 &&
         probe->has_bind_memory2 && probe->has_queue_family_foreign;
}

static int porthole_native_linux_vk_enumerate_instance_extensions(
    PFN_vkEnumerateInstanceExtensionProperties enumerate,
    struct porthole_native_linux_vulkan_probe *out) {
  uint32_t count = 0;
  VkResult result = enumerate(NULL, &count, NULL);
  int error = porthole_native_linux_vk_result(result);
  if (error != 0) {
    return error;
  }
  out->can_enumerate_instance_extensions = 1;
  out->instance_extension_count = count;
  if (count == 0) {
    return 0;
  }
  VkExtensionProperties *properties = calloc(count, sizeof(VkExtensionProperties));
  if (properties == NULL) {
    return ENOMEM;
  }
  result = enumerate(NULL, &count, properties);
  error = porthole_native_linux_vk_result(result);
  if (error == 0) {
    out->instance_extension_count = count;
    for (uint32_t index = 0; index < count; index++) {
      porthole_native_linux_vk_record_instance_extension(&properties[index], out);
    }
  }
  free(properties);
  return error;
}

static int porthole_native_linux_vk_enumerate_device_extensions(
    PFN_vkEnumerateDeviceExtensionProperties enumerate,
    VkPhysicalDevice physical_device,
    struct porthole_native_linux_vulkan_probe *out) {
  uint32_t count = 0;
  VkResult result = enumerate(physical_device, NULL, &count, NULL);
  int error = porthole_native_linux_vk_result(result);
  if (error != 0 || count == 0) {
    return error;
  }
  VkExtensionProperties *properties = calloc(count, sizeof(VkExtensionProperties));
  if (properties == NULL) {
    return ENOMEM;
  }
  result = enumerate(physical_device, NULL, &count, properties);
  error = porthole_native_linux_vk_result(result);
  if (error == 0) {
    for (uint32_t index = 0; index < count; index++) {
      porthole_native_linux_vk_record_device_extension(&properties[index], out);
    }
  }
  free(properties);
  return error;
}

static int porthole_native_linux_vk_physical_device_has_modifier_extension(
    PFN_vkEnumerateDeviceExtensionProperties enumerate,
    VkPhysicalDevice physical_device) {
  struct porthole_native_linux_vulkan_probe probe;
  memset(&probe, 0, sizeof(probe));
  int error = porthole_native_linux_vk_enumerate_device_extensions(enumerate, physical_device, &probe);
  return error == 0 && probe.has_image_drm_format_modifier;
}

static const void *porthole_native_linux_vk_proc(PFN_vkGetInstanceProcAddr get_proc,
                                                 VkInstance instance,
                                                 const char *name) {
  return get_proc == NULL ? NULL : get_proc(instance, name);
}

static const void *porthole_native_linux_vk_device_proc(PFN_vkGetDeviceProcAddr get_proc,
                                                        VkDevice device,
                                                        const char *name) {
  return get_proc == NULL ? NULL : get_proc(device, name);
}

static int porthole_native_linux_vk_first_queue_family(
    PFN_vkGetPhysicalDeviceQueueFamilyProperties get_queue_families,
    VkPhysicalDevice physical_device,
    uint32_t *out_queue_family) {
  if (get_queue_families == NULL || out_queue_family == NULL) {
    return EINVAL;
  }
  uint32_t count = 0;
  get_queue_families(physical_device, &count, NULL);
  if (count == 0) {
    return ENODEV;
  }
  VkQueueFamilyProperties *families = calloc(count, sizeof(VkQueueFamilyProperties));
  if (families == NULL) {
    return ENOMEM;
  }
  get_queue_families(physical_device, &count, families);
  int error = ENODEV;
  for (uint32_t index = 0; index < count; index++) {
    if (families[index].queueCount > 0) {
      *out_queue_family = index;
      error = 0;
      break;
    }
  }
  free(families);
  return error;
}

static int porthole_native_linux_vk_try_reference_device(
    PFN_vkGetInstanceProcAddr get_instance_proc,
    VkInstance instance,
    VkPhysicalDevice physical_device,
    struct porthole_native_linux_vulkan_probe *out) {
  PFN_vkGetPhysicalDeviceQueueFamilyProperties get_queue_families =
      (PFN_vkGetPhysicalDeviceQueueFamilyProperties)porthole_native_linux_vk_proc(
          get_instance_proc, instance, "vkGetPhysicalDeviceQueueFamilyProperties");
  PFN_vkCreateDevice create_device =
      (PFN_vkCreateDevice)porthole_native_linux_vk_proc(get_instance_proc, instance, "vkCreateDevice");
  if (get_queue_families == NULL || create_device == NULL) {
    return 0;
  }

  uint32_t queue_family = 0;
  int error = porthole_native_linux_vk_first_queue_family(get_queue_families, physical_device, &queue_family);
  if (error != 0) {
    return error == ENODEV ? 0 : error;
  }

  const float priority = 1.0f;
  VkDeviceQueueCreateInfo queue_info;
  memset(&queue_info, 0, sizeof(queue_info));
  queue_info.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
  queue_info.queueFamilyIndex = queue_family;
  queue_info.queueCount = 1;
  queue_info.pQueuePriorities = &priority;

  const char *extensions[] = {
      "VK_EXT_external_memory_dma_buf",
      "VK_KHR_external_memory_fd",
      "VK_EXT_image_drm_format_modifier",
      "VK_KHR_get_memory_requirements2",
      "VK_KHR_bind_memory2",
      "VK_EXT_queue_family_foreign",
  };

  VkDeviceCreateInfo create_info;
  memset(&create_info, 0, sizeof(create_info));
  create_info.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
  create_info.queueCreateInfoCount = 1;
  create_info.pQueueCreateInfos = &queue_info;
  create_info.enabledExtensionCount = sizeof(extensions) / sizeof(extensions[0]);
  create_info.ppEnabledExtensionNames = extensions;

  VkDevice device = NULL;
  VkResult result = create_device(physical_device, &create_info, NULL, &device);
  error = porthole_native_linux_vk_result(result);
  if (error != 0 || device == NULL) {
    return error == 0 ? EIO : error;
  }
  out->can_create_reference_device = 1;

  PFN_vkDestroyDevice destroy_device =
      (PFN_vkDestroyDevice)porthole_native_linux_vk_proc(get_instance_proc, instance, "vkDestroyDevice");
  PFN_vkGetDeviceProcAddr get_device_proc =
      (PFN_vkGetDeviceProcAddr)porthole_native_linux_vk_proc(get_instance_proc, instance, "vkGetDeviceProcAddr");
  if (porthole_native_linux_vk_device_proc(get_device_proc, device, "vkCreateImage") != NULL &&
      porthole_native_linux_vk_device_proc(get_device_proc, device, "vkDestroyImage") != NULL &&
      porthole_native_linux_vk_device_proc(get_device_proc, device, "vkAllocateMemory") != NULL &&
      porthole_native_linux_vk_device_proc(get_device_proc, device, "vkFreeMemory") != NULL &&
      porthole_native_linux_vk_device_proc(get_device_proc, device, "vkBindImageMemory2KHR") != NULL &&
      porthole_native_linux_vk_device_proc(get_device_proc, device, "vkGetImageMemoryRequirements2KHR") != NULL &&
      porthole_native_linux_vk_device_proc(get_device_proc, device, "vkGetMemoryFdPropertiesKHR") != NULL) {
    out->has_image_import_device_functions = 1;
  }

  if (destroy_device != NULL) {
    destroy_device(device, NULL);
  }
  return 0;
}

static void porthole_native_linux_vk_close_reference_device(
    struct porthole_native_linux_vk_reference_device *context) {
  if (context == NULL) {
    return;
  }
  if (context->device != NULL && context->destroy_device != NULL) {
    context->destroy_device(context->device, NULL);
  }
  if (context->instance != NULL && context->destroy_instance != NULL) {
    context->destroy_instance(context->instance, NULL);
  }
  if (context->loader != NULL) {
    dlclose(context->loader);
  }
  memset(context, 0, sizeof(*context));
}

static int porthole_native_linux_vk_open_reference_device(
    struct porthole_native_linux_vk_reference_device *context) {
  if (context == NULL) {
    return EINVAL;
  }
  memset(context, 0, sizeof(*context));

  context->loader = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
  if (context->loader == NULL) {
    return ENOSYS;
  }
  context->get_instance_proc = (PFN_vkGetInstanceProcAddr)dlsym(context->loader, "vkGetInstanceProcAddr");
  if (context->get_instance_proc == NULL) {
    porthole_native_linux_vk_close_reference_device(context);
    return ENOSYS;
  }

  PFN_vkCreateInstance create_instance =
      (PFN_vkCreateInstance)porthole_native_linux_vk_proc(context->get_instance_proc, NULL, "vkCreateInstance");
  if (create_instance == NULL) {
    porthole_native_linux_vk_close_reference_device(context);
    return ENOSYS;
  }

  VkApplicationInfo app_info;
  memset(&app_info, 0, sizeof(app_info));
  app_info.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
  app_info.pApplicationName = "porthole-vulkan-reference-import";
  app_info.apiVersion = VK_API_VERSION_1_0;

  VkInstanceCreateInfo instance_info;
  memset(&instance_info, 0, sizeof(instance_info));
  instance_info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
  instance_info.pApplicationInfo = &app_info;

  VkResult result = create_instance(&instance_info, NULL, &context->instance);
  int error = porthole_native_linux_vk_result(result);
  if (error != 0 || context->instance == NULL) {
    porthole_native_linux_vk_close_reference_device(context);
    return error == 0 ? EIO : error;
  }

  context->destroy_instance =
      (PFN_vkDestroyInstance)porthole_native_linux_vk_proc(context->get_instance_proc, context->instance, "vkDestroyInstance");
  PFN_vkEnumeratePhysicalDevices enumerate_physical_devices =
      (PFN_vkEnumeratePhysicalDevices)porthole_native_linux_vk_proc(context->get_instance_proc, context->instance, "vkEnumeratePhysicalDevices");
  PFN_vkEnumerateDeviceExtensionProperties enumerate_device_extensions =
      (PFN_vkEnumerateDeviceExtensionProperties)porthole_native_linux_vk_proc(
          context->get_instance_proc, context->instance, "vkEnumerateDeviceExtensionProperties");
  PFN_vkGetPhysicalDeviceQueueFamilyProperties get_queue_families =
      (PFN_vkGetPhysicalDeviceQueueFamilyProperties)porthole_native_linux_vk_proc(
          context->get_instance_proc, context->instance, "vkGetPhysicalDeviceQueueFamilyProperties");
  PFN_vkGetPhysicalDeviceMemoryProperties get_memory_properties =
      (PFN_vkGetPhysicalDeviceMemoryProperties)porthole_native_linux_vk_proc(
          context->get_instance_proc, context->instance, "vkGetPhysicalDeviceMemoryProperties");
  PFN_vkCreateDevice create_device =
      (PFN_vkCreateDevice)porthole_native_linux_vk_proc(context->get_instance_proc, context->instance, "vkCreateDevice");
  if (enumerate_physical_devices == NULL || enumerate_device_extensions == NULL ||
      get_queue_families == NULL || get_memory_properties == NULL || create_device == NULL) {
    porthole_native_linux_vk_close_reference_device(context);
    return ENOSYS;
  }

  uint32_t physical_device_count = 0;
  result = enumerate_physical_devices(context->instance, &physical_device_count, NULL);
  error = porthole_native_linux_vk_result(result);
  if (error != 0 || physical_device_count == 0) {
    porthole_native_linux_vk_close_reference_device(context);
    return error == 0 ? ENODEV : error;
  }
  VkPhysicalDevice *physical_devices = calloc(physical_device_count, sizeof(VkPhysicalDevice));
  if (physical_devices == NULL) {
    porthole_native_linux_vk_close_reference_device(context);
    return ENOMEM;
  }
  result = enumerate_physical_devices(context->instance, &physical_device_count, physical_devices);
  error = porthole_native_linux_vk_result(result);
  if (error != 0) {
    free(physical_devices);
    porthole_native_linux_vk_close_reference_device(context);
    return error;
  }

  const char *extensions[] = {
      "VK_EXT_external_memory_dma_buf",
      "VK_KHR_external_memory_fd",
      "VK_EXT_image_drm_format_modifier",
      "VK_KHR_get_memory_requirements2",
      "VK_KHR_bind_memory2",
      "VK_EXT_queue_family_foreign",
  };

  for (uint32_t index = 0; index < physical_device_count; index++) {
    struct porthole_native_linux_vulkan_probe device_probe;
    memset(&device_probe, 0, sizeof(device_probe));
    error = porthole_native_linux_vk_enumerate_device_extensions(
        enumerate_device_extensions, physical_devices[index], &device_probe);
    if (error != 0 || !porthole_native_linux_vk_has_reference_device_extensions(&device_probe)) {
      continue;
    }
    uint32_t queue_family = 0;
    error = porthole_native_linux_vk_first_queue_family(get_queue_families, physical_devices[index], &queue_family);
    if (error != 0) {
      continue;
    }

    const float priority = 1.0f;
    VkDeviceQueueCreateInfo queue_info;
    memset(&queue_info, 0, sizeof(queue_info));
    queue_info.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO;
    queue_info.queueFamilyIndex = queue_family;
    queue_info.queueCount = 1;
    queue_info.pQueuePriorities = &priority;

    VkDeviceCreateInfo device_info;
    memset(&device_info, 0, sizeof(device_info));
    device_info.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO;
    device_info.queueCreateInfoCount = 1;
    device_info.pQueueCreateInfos = &queue_info;
    device_info.enabledExtensionCount = sizeof(extensions) / sizeof(extensions[0]);
    device_info.ppEnabledExtensionNames = extensions;

    result = create_device(physical_devices[index], &device_info, NULL, &context->device);
    error = porthole_native_linux_vk_result(result);
    if (error != 0 || context->device == NULL) {
      continue;
    }
    context->physical_device = physical_devices[index];
    get_memory_properties(context->physical_device, &context->memory_properties);
    break;
  }
  free(physical_devices);

  if (context->device == NULL) {
    porthole_native_linux_vk_close_reference_device(context);
    return ENODEV;
  }

  context->destroy_device =
      (PFN_vkDestroyDevice)porthole_native_linux_vk_proc(context->get_instance_proc, context->instance, "vkDestroyDevice");
  context->get_device_proc =
      (PFN_vkGetDeviceProcAddr)porthole_native_linux_vk_proc(context->get_instance_proc, context->instance, "vkGetDeviceProcAddr");
  context->create_image = (PFN_vkCreateImage)porthole_native_linux_vk_device_proc(context->get_device_proc, context->device, "vkCreateImage");
  context->destroy_image = (PFN_vkDestroyImage)porthole_native_linux_vk_device_proc(context->get_device_proc, context->device, "vkDestroyImage");
  context->get_image_memory_requirements2 =
      (PFN_vkGetImageMemoryRequirements2KHR)porthole_native_linux_vk_device_proc(
          context->get_device_proc, context->device, "vkGetImageMemoryRequirements2KHR");
  context->get_memory_fd_properties =
      (PFN_vkGetMemoryFdPropertiesKHR)porthole_native_linux_vk_device_proc(
          context->get_device_proc, context->device, "vkGetMemoryFdPropertiesKHR");
  context->allocate_memory =
      (PFN_vkAllocateMemory)porthole_native_linux_vk_device_proc(context->get_device_proc, context->device, "vkAllocateMemory");
  context->free_memory =
      (PFN_vkFreeMemory)porthole_native_linux_vk_device_proc(context->get_device_proc, context->device, "vkFreeMemory");
  context->bind_image_memory2 =
      (PFN_vkBindImageMemory2KHR)porthole_native_linux_vk_device_proc(context->get_device_proc, context->device, "vkBindImageMemory2KHR");
  if (context->create_image == NULL || context->destroy_image == NULL ||
      context->get_image_memory_requirements2 == NULL || context->get_memory_fd_properties == NULL ||
      context->allocate_memory == NULL || context->free_memory == NULL || context->bind_image_memory2 == NULL) {
    porthole_native_linux_vk_close_reference_device(context);
    return ENOSYS;
  }
  return 0;
}

static int porthole_native_linux_vk_select_memory_type(
    const struct porthole_native_linux_vk_reference_device *context,
    uint32_t memory_type_bits,
    uint32_t *out_index) {
  if (context == NULL || out_index == NULL || memory_type_bits == 0) {
    return EINVAL;
  }
  uint32_t count = context->memory_properties.memoryTypeCount;
  if (count > VK_MAX_MEMORY_TYPES) {
    count = VK_MAX_MEMORY_TYPES;
  }
  for (uint32_t index = 0; index < count; index++) {
    if ((memory_type_bits & (1u << index)) != 0) {
      *out_index = index;
      return 0;
    }
  }
  return ENODEV;
}

int porthole_native_linux_vulkan_import_dmabuf_image(
    const struct porthole_native_linux_vulkan_import_image *image) {
  if (image == NULL || image->struct_size < sizeof(*image) || image->fd < 0 ||
      image->width == 0 || image->height == 0 || image->plane_count != 1 ||
      (image->vk_format != VK_FORMAT_R8G8B8A8_UNORM && image->vk_format != VK_FORMAT_B8G8R8A8_UNORM) ||
      image->planes[0].stride == 0) {
    return EINVAL;
  }

  struct porthole_native_linux_vk_reference_device context;
  int error = porthole_native_linux_vk_open_reference_device(&context);
  if (error != 0) {
    return error;
  }

  VkSubresourceLayout plane_layout;
  memset(&plane_layout, 0, sizeof(plane_layout));
  plane_layout.offset = image->planes[0].offset;
  plane_layout.rowPitch = image->planes[0].stride;
  plane_layout.size = (VkDeviceSize)image->planes[0].stride * image->height;

  VkImageDrmFormatModifierExplicitCreateInfoEXT modifier_info;
  memset(&modifier_info, 0, sizeof(modifier_info));
  modifier_info.sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT;
  modifier_info.drmFormatModifier = image->modifier;
  modifier_info.drmFormatModifierPlaneCount = 1;
  modifier_info.pPlaneLayouts = &plane_layout;

  VkExternalMemoryImageCreateInfo external_info;
  memset(&external_info, 0, sizeof(external_info));
  external_info.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO;
  external_info.pNext = &modifier_info;
  external_info.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;

  VkImageCreateInfo image_info;
  memset(&image_info, 0, sizeof(image_info));
  image_info.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO;
  image_info.pNext = &external_info;
  image_info.imageType = VK_IMAGE_TYPE_2D;
  image_info.format = image->vk_format;
  image_info.extent.width = image->width;
  image_info.extent.height = image->height;
  image_info.extent.depth = 1;
  image_info.mipLevels = 1;
  image_info.arrayLayers = 1;
  image_info.samples = VK_SAMPLE_COUNT_1_BIT;
  image_info.tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT;
  image_info.usage = VK_IMAGE_USAGE_SAMPLED_BIT;
  image_info.sharingMode = VK_SHARING_MODE_EXCLUSIVE;
  image_info.initialLayout = VK_IMAGE_LAYOUT_UNDEFINED;

  VkImage vk_image = NULL;
  VkResult result = context.create_image(context.device, &image_info, NULL, &vk_image);
  error = porthole_native_linux_vk_result(result);
  if (error != 0 || vk_image == NULL) {
    porthole_native_linux_vk_close_reference_device(&context);
    return error == 0 ? EIO : error;
  }

  int import_fd = dup(image->fd);
  if (import_fd < 0) {
    context.destroy_image(context.device, vk_image, NULL);
    porthole_native_linux_vk_close_reference_device(&context);
    return errno;
  }

  VkMemoryDedicatedRequirements dedicated_requirements;
  memset(&dedicated_requirements, 0, sizeof(dedicated_requirements));
  dedicated_requirements.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_REQUIREMENTS;

  VkMemoryRequirements2 memory_requirements;
  memset(&memory_requirements, 0, sizeof(memory_requirements));
  memory_requirements.sType = VK_STRUCTURE_TYPE_MEMORY_REQUIREMENTS_2;
  memory_requirements.pNext = &dedicated_requirements;

  VkImageMemoryRequirementsInfo2 requirements_info;
  memset(&requirements_info, 0, sizeof(requirements_info));
  requirements_info.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_REQUIREMENTS_INFO_2;
  requirements_info.image = vk_image;
  context.get_image_memory_requirements2(context.device, &requirements_info, &memory_requirements);

  VkMemoryFdPropertiesKHR fd_properties;
  memset(&fd_properties, 0, sizeof(fd_properties));
  fd_properties.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR;
  result = context.get_memory_fd_properties(
      context.device, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, import_fd, &fd_properties);
  error = porthole_native_linux_vk_result(result);
  if (error != 0) {
    close(import_fd);
    context.destroy_image(context.device, vk_image, NULL);
    porthole_native_linux_vk_close_reference_device(&context);
    return error;
  }

  uint32_t memory_type_index = 0;
  error = porthole_native_linux_vk_select_memory_type(
      &context, memory_requirements.memoryRequirements.memoryTypeBits & fd_properties.memoryTypeBits, &memory_type_index);
  if (error != 0) {
    close(import_fd);
    context.destroy_image(context.device, vk_image, NULL);
    porthole_native_linux_vk_close_reference_device(&context);
    return error;
  }

  VkMemoryDedicatedAllocateInfo dedicated_allocate;
  memset(&dedicated_allocate, 0, sizeof(dedicated_allocate));
  dedicated_allocate.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO;
  dedicated_allocate.image = vk_image;

  VkImportMemoryFdInfoKHR import_info;
  memset(&import_info, 0, sizeof(import_info));
  import_info.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR;
  import_info.pNext = (dedicated_requirements.prefersDedicatedAllocation ||
                       dedicated_requirements.requiresDedicatedAllocation)
                          ? &dedicated_allocate
                          : NULL;
  import_info.handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT;
  import_info.fd = import_fd;

  VkMemoryAllocateInfo allocate_info;
  memset(&allocate_info, 0, sizeof(allocate_info));
  allocate_info.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO;
  allocate_info.pNext = &import_info;
  allocate_info.allocationSize = memory_requirements.memoryRequirements.size;
  allocate_info.memoryTypeIndex = memory_type_index;

  VkDeviceMemory memory = NULL;
  result = context.allocate_memory(context.device, &allocate_info, NULL, &memory);
  error = porthole_native_linux_vk_result(result);
  if (error != 0 || memory == NULL) {
    close(import_fd);
    context.destroy_image(context.device, vk_image, NULL);
    porthole_native_linux_vk_close_reference_device(&context);
    return error == 0 ? EIO : error;
  }
  import_fd = -1; // Vulkan owns the duplicated fd after successful import.

  VkBindImageMemoryInfo bind_info;
  memset(&bind_info, 0, sizeof(bind_info));
  bind_info.sType = VK_STRUCTURE_TYPE_BIND_IMAGE_MEMORY_INFO;
  bind_info.image = vk_image;
  bind_info.memory = memory;
  bind_info.memoryOffset = 0;
  result = context.bind_image_memory2(context.device, 1, &bind_info);
  error = porthole_native_linux_vk_result(result);

  context.free_memory(context.device, memory, NULL);
  context.destroy_image(context.device, vk_image, NULL);
  porthole_native_linux_vk_close_reference_device(&context);
  return error;
}

int porthole_native_linux_vulkan_query_format_modifiers(
    struct porthole_native_linux_vulkan_modifier_query *query) {
  if (query == NULL || query->struct_size < sizeof(*query) ||
      query->modifiers == NULL || query->modifier_capacity == 0 ||
      (query->vk_format != VK_FORMAT_R8G8B8A8_UNORM && query->vk_format != VK_FORMAT_B8G8R8A8_UNORM)) {
    return EINVAL;
  }
  query->modifier_count = 0;

  void *loader = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
  if (loader == NULL) {
    return ENOSYS;
  }
  PFN_vkGetInstanceProcAddr get_proc = (PFN_vkGetInstanceProcAddr)dlsym(loader, "vkGetInstanceProcAddr");
  if (get_proc == NULL) {
    dlclose(loader);
    return ENOSYS;
  }
  PFN_vkEnumerateInstanceExtensionProperties enumerate_instance_extensions =
      (PFN_vkEnumerateInstanceExtensionProperties)dlsym(loader, "vkEnumerateInstanceExtensionProperties");
  if (enumerate_instance_extensions == NULL) {
    enumerate_instance_extensions = (PFN_vkEnumerateInstanceExtensionProperties)porthole_native_linux_vk_proc(
        get_proc, NULL, "vkEnumerateInstanceExtensionProperties");
  }
  PFN_vkCreateInstance create_instance =
      (PFN_vkCreateInstance)porthole_native_linux_vk_proc(get_proc, NULL, "vkCreateInstance");
  if (enumerate_instance_extensions == NULL || create_instance == NULL) {
    dlclose(loader);
    return ENOSYS;
  }

  struct porthole_native_linux_vulkan_probe probe;
  memset(&probe, 0, sizeof(probe));
  int error = porthole_native_linux_vk_enumerate_instance_extensions(enumerate_instance_extensions, &probe);
  if (error != 0) {
    dlclose(loader);
    return error;
  }
  if (!probe.has_get_physical_device_properties2) {
    dlclose(loader);
    return ENOSYS;
  }

  const char *instance_extensions[] = {
      "VK_KHR_get_physical_device_properties2",
  };
  VkApplicationInfo app_info;
  memset(&app_info, 0, sizeof(app_info));
  app_info.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
  app_info.pApplicationName = "porthole-vulkan-modifier-query";
  app_info.apiVersion = VK_API_VERSION_1_0;

  VkInstanceCreateInfo create_info;
  memset(&create_info, 0, sizeof(create_info));
  create_info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
  create_info.pApplicationInfo = &app_info;
  create_info.enabledExtensionCount = 1;
  create_info.ppEnabledExtensionNames = instance_extensions;

  VkInstance instance = NULL;
  VkResult result = create_instance(&create_info, NULL, &instance);
  if (result != VK_SUCCESS || instance == NULL) {
    dlclose(loader);
    return result == VK_SUCCESS ? EIO : porthole_native_linux_vk_result(result);
  }

  PFN_vkDestroyInstance destroy_instance =
      (PFN_vkDestroyInstance)porthole_native_linux_vk_proc(get_proc, instance, "vkDestroyInstance");
  PFN_vkEnumeratePhysicalDevices enumerate_physical_devices =
      (PFN_vkEnumeratePhysicalDevices)porthole_native_linux_vk_proc(get_proc, instance, "vkEnumeratePhysicalDevices");
  PFN_vkEnumerateDeviceExtensionProperties enumerate_device_extensions =
      (PFN_vkEnumerateDeviceExtensionProperties)porthole_native_linux_vk_proc(
          get_proc, instance, "vkEnumerateDeviceExtensionProperties");
  PFN_vkGetPhysicalDeviceFormatProperties2KHR get_format_properties2 =
      (PFN_vkGetPhysicalDeviceFormatProperties2KHR)porthole_native_linux_vk_proc(
          get_proc, instance, "vkGetPhysicalDeviceFormatProperties2");
  if (get_format_properties2 == NULL) {
    get_format_properties2 = (PFN_vkGetPhysicalDeviceFormatProperties2KHR)porthole_native_linux_vk_proc(
        get_proc, instance, "vkGetPhysicalDeviceFormatProperties2KHR");
  }
  if (enumerate_physical_devices == NULL || enumerate_device_extensions == NULL ||
      get_format_properties2 == NULL) {
    if (destroy_instance != NULL) {
      destroy_instance(instance, NULL);
    }
    dlclose(loader);
    return ENOSYS;
  }

  uint32_t physical_device_count = 0;
  result = enumerate_physical_devices(instance, &physical_device_count, NULL);
  error = porthole_native_linux_vk_result(result);
  if (error != 0 || physical_device_count == 0) {
    if (destroy_instance != NULL) {
      destroy_instance(instance, NULL);
    }
    dlclose(loader);
    return error == 0 ? ENODEV : error;
  }
  VkPhysicalDevice *physical_devices = calloc(physical_device_count, sizeof(VkPhysicalDevice));
  if (physical_devices == NULL) {
    if (destroy_instance != NULL) {
      destroy_instance(instance, NULL);
    }
    dlclose(loader);
    return ENOMEM;
  }
  result = enumerate_physical_devices(instance, &physical_device_count, physical_devices);
  error = porthole_native_linux_vk_result(result);
  if (error != 0) {
    free(physical_devices);
    if (destroy_instance != NULL) {
      destroy_instance(instance, NULL);
    }
    dlclose(loader);
    return error;
  }

  for (uint32_t device_index = 0; device_index < physical_device_count; device_index++) {
    if (!porthole_native_linux_vk_physical_device_has_modifier_extension(
            enumerate_device_extensions, physical_devices[device_index])) {
      continue;
    }

    VkDrmFormatModifierPropertiesListEXT modifier_list;
    memset(&modifier_list, 0, sizeof(modifier_list));
    modifier_list.sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT;

    VkFormatProperties2 properties;
    memset(&properties, 0, sizeof(properties));
    properties.sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2;
    properties.pNext = &modifier_list;
    get_format_properties2(physical_devices[device_index], query->vk_format, &properties);
    if (modifier_list.drmFormatModifierCount == 0) {
      continue;
    }

    VkDrmFormatModifierPropertiesEXT *modifiers =
        calloc(modifier_list.drmFormatModifierCount, sizeof(VkDrmFormatModifierPropertiesEXT));
    if (modifiers == NULL) {
      free(physical_devices);
      if (destroy_instance != NULL) {
        destroy_instance(instance, NULL);
      }
      dlclose(loader);
      return ENOMEM;
    }
    modifier_list.pDrmFormatModifierProperties = modifiers;
    get_format_properties2(physical_devices[device_index], query->vk_format, &properties);
    for (uint32_t index = 0; index < modifier_list.drmFormatModifierCount; index++) {
      if ((modifiers[index].drmFormatModifierTilingFeatures & VK_FORMAT_FEATURE_SAMPLED_IMAGE_BIT) == 0) {
        continue;
      }
      int already_present = 0;
      for (uint32_t existing = 0; existing < query->modifier_count; existing++) {
        if (query->modifiers[existing] == modifiers[index].drmFormatModifier) {
          already_present = 1;
          break;
        }
      }
      if (!already_present && query->modifier_count < query->modifier_capacity) {
        query->modifiers[query->modifier_count++] = modifiers[index].drmFormatModifier;
      }
    }
    free(modifiers);
    if (query->modifier_count >= query->modifier_capacity) {
      break;
    }
  }

  free(physical_devices);
  if (destroy_instance != NULL) {
    destroy_instance(instance, NULL);
  }
  dlclose(loader);
  return 0;
}

int porthole_native_linux_vulkan_probe(struct porthole_native_linux_vulkan_probe *out) {
  if (out == NULL || out->struct_size < sizeof(*out)) {
    return EINVAL;
  }
  memset(((char *)out) + sizeof(uint32_t), 0, sizeof(*out) - sizeof(uint32_t));

  void *loader = dlopen("libvulkan.so.1", RTLD_NOW | RTLD_LOCAL);
  if (loader == NULL) {
    return 0;
  }
  out->loader_present = 1;

  PFN_vkGetInstanceProcAddr get_proc = (PFN_vkGetInstanceProcAddr)dlsym(loader, "vkGetInstanceProcAddr");
  if (get_proc == NULL) {
    dlclose(loader);
    return 0;
  }

  PFN_vkEnumerateInstanceExtensionProperties enumerate_instance_extensions =
      (PFN_vkEnumerateInstanceExtensionProperties)dlsym(loader, "vkEnumerateInstanceExtensionProperties");
  if (enumerate_instance_extensions == NULL) {
    enumerate_instance_extensions = (PFN_vkEnumerateInstanceExtensionProperties)porthole_native_linux_vk_proc(
        get_proc, NULL, "vkEnumerateInstanceExtensionProperties");
  }
  if (enumerate_instance_extensions == NULL) {
    dlclose(loader);
    return 0;
  }

  int error = porthole_native_linux_vk_enumerate_instance_extensions(enumerate_instance_extensions, out);
  if (error != 0) {
    dlclose(loader);
    return error;
  }

  PFN_vkCreateInstance create_instance =
      (PFN_vkCreateInstance)porthole_native_linux_vk_proc(get_proc, NULL, "vkCreateInstance");
  if (create_instance == NULL) {
    dlclose(loader);
    return 0;
  }

  VkApplicationInfo app_info;
  memset(&app_info, 0, sizeof(app_info));
  app_info.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO;
  app_info.pApplicationName = "porthole-vulkan-probe";
  app_info.apiVersion = VK_API_VERSION_1_0;

  VkInstanceCreateInfo create_info;
  memset(&create_info, 0, sizeof(create_info));
  create_info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
  create_info.pApplicationInfo = &app_info;

  VkInstance instance = NULL;
  VkResult result = create_instance(&create_info, NULL, &instance);
  if (result != VK_SUCCESS || instance == NULL) {
    dlclose(loader);
    return 0;
  }
  out->can_create_instance = 1;

  PFN_vkDestroyInstance destroy_instance =
      (PFN_vkDestroyInstance)porthole_native_linux_vk_proc(get_proc, instance, "vkDestroyInstance");
  PFN_vkEnumeratePhysicalDevices enumerate_physical_devices =
      (PFN_vkEnumeratePhysicalDevices)porthole_native_linux_vk_proc(get_proc, instance, "vkEnumeratePhysicalDevices");
  PFN_vkEnumerateDeviceExtensionProperties enumerate_device_extensions =
      (PFN_vkEnumerateDeviceExtensionProperties)porthole_native_linux_vk_proc(
          get_proc, instance, "vkEnumerateDeviceExtensionProperties");

  if (enumerate_physical_devices != NULL && enumerate_device_extensions != NULL) {
    uint32_t physical_device_count = 0;
    result = enumerate_physical_devices(instance, &physical_device_count, NULL);
    error = porthole_native_linux_vk_result(result);
    if (error == 0) {
      out->physical_device_count = physical_device_count;
      if (physical_device_count > 0) {
        VkPhysicalDevice *physical_devices = calloc(physical_device_count, sizeof(VkPhysicalDevice));
        if (physical_devices == NULL) {
          error = ENOMEM;
        } else {
          result = enumerate_physical_devices(instance, &physical_device_count, physical_devices);
          error = porthole_native_linux_vk_result(result);
          if (error == 0) {
            out->physical_device_count = physical_device_count;
            for (uint32_t index = 0; index < physical_device_count; index++) {
              struct porthole_native_linux_vulkan_probe device_probe;
              memset(&device_probe, 0, sizeof(device_probe));
              error = porthole_native_linux_vk_enumerate_device_extensions(
                  enumerate_device_extensions, physical_devices[index], &device_probe);
              if (error != 0) {
                break;
              }
              out->has_external_memory_dma_buf |= device_probe.has_external_memory_dma_buf;
              out->has_external_memory_fd |= device_probe.has_external_memory_fd;
              out->has_image_drm_format_modifier |= device_probe.has_image_drm_format_modifier;
              out->has_get_memory_requirements2 |= device_probe.has_get_memory_requirements2;
              out->has_bind_memory2 |= device_probe.has_bind_memory2;
              out->has_queue_family_foreign |= device_probe.has_queue_family_foreign;
              out->has_external_semaphore_fd |= device_probe.has_external_semaphore_fd;
              out->has_external_fence_fd |= device_probe.has_external_fence_fd;
              out->has_timeline_semaphore |= device_probe.has_timeline_semaphore;
              if (!out->can_create_reference_device &&
                  porthole_native_linux_vk_has_reference_device_extensions(&device_probe)) {
                error = porthole_native_linux_vk_try_reference_device(
                    get_proc, instance, physical_devices[index], out);
                if (error != 0) {
                  break;
                }
              }
            }
          }
          free(physical_devices);
        }
      }
    }
  }

  if (destroy_instance != NULL) {
    destroy_instance(instance, NULL);
  }
  dlclose(loader);
  return error;
}
