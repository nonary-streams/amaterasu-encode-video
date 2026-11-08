// RGB to P010 conversion for CUDA resource input.
//
// Reproduces the GL luma and chroma passes in convert.c: the same matrix rows
// (computed and checked in color.rs), clamp, 10-bit round-to-nearest code
// stored in the high bits of each 16-bit sample, and chroma as the box
// average of each 2x2 block (centre sited). Writes the NV12-style layout the
// encoder registers with NVENC: luma rows, then interleaved Cb/Cr rows, at
// one pitch. Regenerate convert.ptx with scripts/generate-cuda-ptx.sh.

struct Rows {
    float4 luma, cb, cr;
};

__device__ static float3 unpack(unsigned int word) {
    return make_float3((float)(word & 1023u) / 1023.0f, (float)((word >> 10) & 1023u) / 1023.0f,
                       (float)((word >> 20) & 1023u) / 1023.0f);
}

__device__ static unsigned short p010(float row_dot) {
    const float code = fminf(fmaxf(row_dot, 0.0f), 1.0f);
    return (unsigned short)(floorf(code * 1023.0f + 0.5f)) << 6;
}

__device__ static float apply(float4 row, float3 rgb) {
    return row.x * rgb.x + row.y * rgb.y + row.z * rgb.z + row.w;
}

// One thread per 2x2 block: four luma samples and one Cb/Cr pair.
extern "C" __global__ void am_nvenc_x2b10g10r10_to_p010(const unsigned char *source,
                                                        unsigned int source_pitch,
                                                        unsigned char *target,
                                                        unsigned int target_pitch, int width,
                                                        int height, Rows rows) {
    const int block_x = blockIdx.x * blockDim.x + threadIdx.x;
    const int block_y = blockIdx.y * blockDim.y + threadIdx.y;
    const int x = block_x * 2;
    const int y = block_y * 2;
    if (x >= width || y >= height)
        return;
    float3 sum = make_float3(0.0f, 0.0f, 0.0f);
    for (int dy = 0; dy < 2; ++dy) {
        const unsigned int *line =
            reinterpret_cast<const unsigned int *>(source + (size_t)(y + dy) * source_pitch);
        unsigned short *luma =
            reinterpret_cast<unsigned short *>(target + (size_t)(y + dy) * target_pitch);
        for (int dx = 0; dx < 2; ++dx) {
            const float3 rgb = unpack(line[x + dx]);
            luma[x + dx] = p010(apply(rows.luma, rgb));
            sum.x += rgb.x;
            sum.y += rgb.y;
            sum.z += rgb.z;
        }
    }
    const float3 mean = make_float3(sum.x * 0.25f, sum.y * 0.25f, sum.z * 0.25f);
    unsigned short *chroma = reinterpret_cast<unsigned short *>(
        target + (size_t)height * target_pitch + (size_t)block_y * target_pitch);
    chroma[x] = p010(apply(rows.cb, mean));
    chroma[x + 1] = p010(apply(rows.cr, mean));
}
