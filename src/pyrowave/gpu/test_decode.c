/* Independent GPU decoder readback for HDR precision qualification only.
 * This file is absent from production pyrowave-gpu builds. */
#define _GNU_SOURCE
#include "adapter.h"
#include <dlfcn.h>
#include <stdlib.h>
#include <string.h>

struct am_pyrowave_test_decoder_api {
    void (*get_handles)(pyrowave_device,VkInstance *,VkPhysicalDevice *,VkDevice *);
    pyrowave_result (*create)(const pyrowave_decoder_create_info *,pyrowave_decoder *);
    pyrowave_result (*push)(pyrowave_decoder,const void *,size_t);
    bool (*ready)(pyrowave_decoder,bool);
    pyrowave_result (*decode)(pyrowave_decoder,const pyrowave_gpu_sync_operation *,const pyrowave_gpu_sync_operation *,const pyrowave_gpu_buffers *);
    void (*destroy)(pyrowave_decoder);
};
static int memory_type(const VkPhysicalDeviceMemoryProperties *memory,uint32_t bits,VkMemoryPropertyFlags required) {
    for (uint32_t i=0;i<memory->memoryTypeCount;i++) if ((bits&(1u<<i)) && (memory->memoryTypes[i].propertyFlags&required)==required) return (int)i;
    return -1;
}
int am_pyrowave_test_decode16(const struct am_pyrowave_test_decoder_api *api,void *pyro_device,const uint8_t *records,size_t records_size,uint32_t width,uint32_t height,uint16_t *output,size_t output_len) {
    if (!api || !pyro_device || !records || records_size<4 || !output || !width || !height || output_len!=(size_t)width*height*3/2) return -1;
    int result=-1;
    void *loader=dlopen("libvulkan.so.1",RTLD_NOW|RTLD_LOCAL);
    if (!loader) return -1;
    PFN_vkGetInstanceProcAddr get=(PFN_vkGetInstanceProcAddr)dlsym(loader,"vkGetInstanceProcAddr");
    VkInstance instance=VK_NULL_HANDLE;VkPhysicalDevice physical=VK_NULL_HANDLE;VkDevice device=VK_NULL_HANDLE;
    api->get_handles(pyro_device,&instance,&physical,&device);
    if (!get || !instance || !physical || !device) {dlclose(loader);return -1;}
    PFN_vkGetDeviceProcAddr get_device=(PFN_vkGetDeviceProcAddr)get(instance,"vkGetDeviceProcAddr");
    PFN_vkGetPhysicalDeviceQueueFamilyProperties queues=(PFN_vkGetPhysicalDeviceQueueFamilyProperties)get(instance,"vkGetPhysicalDeviceQueueFamilyProperties");
    PFN_vkGetPhysicalDeviceMemoryProperties memory_properties=(PFN_vkGetPhysicalDeviceMemoryProperties)get(instance,"vkGetPhysicalDeviceMemoryProperties");
    if (!get_device || !queues || !memory_properties) {dlclose(loader);return -1;}
#define PROC(name) PFN_##name name=(PFN_##name)get_device(device,#name); if (!name) {dlclose(loader);return -1;}
    PROC(vkGetDeviceQueue) PROC(vkCreateCommandPool) PROC(vkDestroyCommandPool) PROC(vkAllocateCommandBuffers) PROC(vkResetCommandPool)
    PROC(vkBeginCommandBuffer) PROC(vkEndCommandBuffer) PROC(vkCmdPipelineBarrier) PROC(vkCmdCopyImageToBuffer)
    PROC(vkQueueSubmit) PROC(vkQueueWaitIdle) PROC(vkDeviceWaitIdle)
    PROC(vkCreateImage) PROC(vkDestroyImage) PROC(vkGetImageMemoryRequirements) PROC(vkAllocateMemory) PROC(vkFreeMemory) PROC(vkBindImageMemory)
    PROC(vkCreateBuffer) PROC(vkDestroyBuffer) PROC(vkGetBufferMemoryRequirements) PROC(vkBindBufferMemory) PROC(vkMapMemory) PROC(vkUnmapMemory)
    VkImage images[3]={VK_NULL_HANDLE,VK_NULL_HANDLE,VK_NULL_HANDLE};VkDeviceMemory allocations[3]={VK_NULL_HANDLE,VK_NULL_HANDLE,VK_NULL_HANDLE};
    VkBuffer buffer=VK_NULL_HANDLE;VkDeviceMemory buffer_memory=VK_NULL_HANDLE;VkCommandPool pool=VK_NULL_HANDLE;
    pyrowave_decoder decoder=NULL;void *mapped=NULL;
    uint32_t queue_count=0;queues(physical,&queue_count,NULL);
    if (!queue_count || queue_count>128) goto cleanup;
    VkQueueFamilyProperties properties[128];queues(physical,&queue_count,properties);
    uint32_t family=0;
    while(family<queue_count && !(properties[family].queueFlags&VK_QUEUE_GRAPHICS_BIT)) family++;
    if (family==queue_count) goto cleanup;
    VkQueue queue=VK_NULL_HANDLE;vkGetDeviceQueue(device,family,0,&queue);
    if (!queue) goto cleanup;
    VkPhysicalDeviceMemoryProperties memory;memory_properties(physical,&memory);
    VkCommandPoolCreateInfo pool_info={.sType=VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,.queueFamilyIndex=family};
    if (vkCreateCommandPool(device,&pool_info,NULL,&pool)!=VK_SUCCESS) goto cleanup;
    VkCommandBuffer command=VK_NULL_HANDLE;
    VkCommandBufferAllocateInfo command_info={.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,.commandPool=pool,.level=VK_COMMAND_BUFFER_LEVEL_PRIMARY,.commandBufferCount=1};
    if (vkAllocateCommandBuffers(device,&command_info,&command)!=VK_SUCCESS) goto cleanup;
    VkImageMemoryBarrier barriers[3];memset(barriers,0,sizeof(barriers));
    pyrowave_gpu_buffers targets;memset(&targets,0,sizeof(targets));
    for (uint32_t i=0;i<3;i++) {
        uint32_t w=i?width/2:width,h=i?height/2:height;
        VkImageCreateInfo image_info={.sType=VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,.imageType=VK_IMAGE_TYPE_2D,.format=VK_FORMAT_R16_UNORM,.extent={w,h,1},.mipLevels=1,.arrayLayers=1,.samples=VK_SAMPLE_COUNT_1_BIT,.tiling=VK_IMAGE_TILING_OPTIMAL,.usage=VK_IMAGE_USAGE_STORAGE_BIT|VK_IMAGE_USAGE_TRANSFER_SRC_BIT,.sharingMode=VK_SHARING_MODE_EXCLUSIVE,.initialLayout=VK_IMAGE_LAYOUT_UNDEFINED};
        if (vkCreateImage(device,&image_info,NULL,&images[i])!=VK_SUCCESS) goto cleanup;
        VkMemoryRequirements requirements;vkGetImageMemoryRequirements(device,images[i],&requirements);
        int type=memory_type(&memory,requirements.memoryTypeBits,VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT);
        if(type<0) goto cleanup;
        VkMemoryAllocateInfo allocation={.sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,.allocationSize=requirements.size,.memoryTypeIndex=(uint32_t)type};
        if(vkAllocateMemory(device,&allocation,NULL,&allocations[i])!=VK_SUCCESS || vkBindImageMemory(device,images[i],allocations[i],0)!=VK_SUCCESS) goto cleanup;
        barriers[i]=(VkImageMemoryBarrier){.sType=VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,.dstAccessMask=VK_ACCESS_SHADER_WRITE_BIT,.oldLayout=VK_IMAGE_LAYOUT_UNDEFINED,.newLayout=VK_IMAGE_LAYOUT_GENERAL,.srcQueueFamilyIndex=VK_QUEUE_FAMILY_IGNORED,.dstQueueFamilyIndex=VK_QUEUE_FAMILY_IGNORED,.image=images[i],.subresourceRange={VK_IMAGE_ASPECT_COLOR_BIT,0,1,0,1}};
        targets.planes[i]=(pyrowave_image_view){.image=images[i],.width=w,.height=h,.image_format=VK_FORMAT_R16_UNORM,.view_format=VK_FORMAT_R16_UNORM,.aspect=VK_IMAGE_ASPECT_COLOR_BIT,.swizzle=VK_COMPONENT_SWIZZLE_IDENTITY,.layout=VK_IMAGE_LAYOUT_GENERAL};
    }
    VkCommandBufferBeginInfo begin={.sType=VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,.flags=VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
    VkSubmitInfo submit={.sType=VK_STRUCTURE_TYPE_SUBMIT_INFO,.commandBufferCount=1,.pCommandBuffers=&command};
    if(vkBeginCommandBuffer(command,&begin)!=VK_SUCCESS) goto cleanup;
    vkCmdPipelineBarrier(command,VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,0,0,NULL,0,NULL,3,barriers);
    if(vkEndCommandBuffer(command)!=VK_SUCCESS || vkQueueSubmit(queue,1,&submit,VK_NULL_HANDLE)!=VK_SUCCESS || vkQueueWaitIdle(queue)!=VK_SUCCESS) goto cleanup;
    pyrowave_decoder_create_info decoder_info={.device=pyro_device,.width=(int)width,.height=(int)height,.chroma=PYROWAVE_CHROMA_SUBSAMPLING_420};
    if(api->create(&decoder_info,&decoder)!=PYROWAVE_SUCCESS) goto cleanup;
    uint32_t packet_count=(uint32_t)records[0]|((uint32_t)records[1]<<8)|((uint32_t)records[2]<<16)|((uint32_t)records[3]<<24);
    size_t offset=4;
    for(uint32_t i=0;i<packet_count;i++) {
        if(records_size-offset<4) goto cleanup;
        uint32_t size=(uint32_t)records[offset]|((uint32_t)records[offset+1]<<8)|((uint32_t)records[offset+2]<<16)|((uint32_t)records[offset+3]<<24);offset+=4;
        if(size>records_size-offset || api->push(decoder,records+offset,size)!=PYROWAVE_SUCCESS) goto cleanup;
        offset+=size;
    }
    if(offset!=records_size || !api->ready(decoder,false) || api->decode(decoder,NULL,NULL,&targets)!=PYROWAVE_SUCCESS || vkDeviceWaitIdle(device)!=VK_SUCCESS) goto cleanup;
    api->destroy(decoder);decoder=NULL;
    if (vkDeviceWaitIdle(device)!=VK_SUCCESS) goto cleanup;
    VkDeviceSize total=output_len*sizeof(uint16_t);
    VkBufferCreateInfo buffer_info={.sType=VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,.size=total,.usage=VK_BUFFER_USAGE_TRANSFER_DST_BIT,.sharingMode=VK_SHARING_MODE_EXCLUSIVE};
    if(vkCreateBuffer(device,&buffer_info,NULL,&buffer)!=VK_SUCCESS) goto cleanup;
    VkMemoryRequirements requirements;vkGetBufferMemoryRequirements(device,buffer,&requirements);
    int type=memory_type(&memory,requirements.memoryTypeBits,VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT|VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
    if(type<0) goto cleanup;
    VkMemoryAllocateInfo allocation={.sType=VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,.allocationSize=requirements.size,.memoryTypeIndex=(uint32_t)type};
    if(vkAllocateMemory(device,&allocation,NULL,&buffer_memory)!=VK_SUCCESS || vkBindBufferMemory(device,buffer,buffer_memory,0)!=VK_SUCCESS || vkResetCommandPool(device,pool,0)!=VK_SUCCESS || vkBeginCommandBuffer(command,&begin)!=VK_SUCCESS) goto cleanup;
    for(uint32_t i=0;i<3;i++) {barriers[i].srcAccessMask=VK_ACCESS_SHADER_WRITE_BIT;barriers[i].dstAccessMask=VK_ACCESS_TRANSFER_READ_BIT;barriers[i].oldLayout=VK_IMAGE_LAYOUT_GENERAL;barriers[i].newLayout=VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;}
    vkCmdPipelineBarrier(command,VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,VK_PIPELINE_STAGE_TRANSFER_BIT,0,0,NULL,0,NULL,3,barriers);
    VkDeviceSize destination=0;
    for(uint32_t i=0;i<3;i++) {
        uint32_t w=i?width/2:width,h=i?height/2:height;
        VkBufferImageCopy copy={.bufferOffset=destination,.imageSubresource={VK_IMAGE_ASPECT_COLOR_BIT,0,0,1},.imageExtent={w,h,1}};
        vkCmdCopyImageToBuffer(command,images[i],VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,buffer,1,&copy);
        destination+=(VkDeviceSize)w*h*sizeof(uint16_t);
    }
    VkBufferMemoryBarrier host={.sType=VK_STRUCTURE_TYPE_BUFFER_MEMORY_BARRIER,.srcAccessMask=VK_ACCESS_TRANSFER_WRITE_BIT,.dstAccessMask=VK_ACCESS_HOST_READ_BIT,.srcQueueFamilyIndex=VK_QUEUE_FAMILY_IGNORED,.dstQueueFamilyIndex=VK_QUEUE_FAMILY_IGNORED,.buffer=buffer,.size=VK_WHOLE_SIZE};
    vkCmdPipelineBarrier(command,VK_PIPELINE_STAGE_TRANSFER_BIT,VK_PIPELINE_STAGE_HOST_BIT,0,0,NULL,1,&host,0,NULL);
    if(vkEndCommandBuffer(command)!=VK_SUCCESS || vkQueueSubmit(queue,1,&submit,VK_NULL_HANDLE)!=VK_SUCCESS || vkQueueWaitIdle(queue)!=VK_SUCCESS || vkMapMemory(device,buffer_memory,0,total,0,&mapped)!=VK_SUCCESS) goto cleanup;
    memcpy(output,mapped,(size_t)total);result=0;
cleanup:
    /* Test cleanup also refuses to destroy borrowed GPU resources without a
     * completion proof, including when decoding or readback fails. */
    if(vkDeviceWaitIdle(device)!=VK_SUCCESS) abort();
    if(mapped) vkUnmapMemory(device,buffer_memory);
    if(decoder) api->destroy(decoder);
    if(buffer) vkDestroyBuffer(device,buffer,NULL);
    if(buffer_memory) vkFreeMemory(device,buffer_memory,NULL);
    if(pool) vkDestroyCommandPool(device,pool,NULL);
    for(uint32_t i=0;i<3;i++) {if(images[i])vkDestroyImage(device,images[i],NULL);if(allocations[i])vkFreeMemory(device,allocations[i],NULL);}
    dlclose(loader);return result;
}
