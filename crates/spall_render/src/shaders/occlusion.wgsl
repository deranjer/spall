// Depth-only proxy for the occlusion test of a terrain chunk: its bounding box,
// drawn against the depth of the scene so far. A GPU occlusion query around the
// draw counts the samples that pass; none means the chunk is hidden. No colour
// is written and no fragment stage runs.
struct Globals {
    view_proj: mat4x4<f32>,
};
@group(0) @binding(0) var<uniform> globals: Globals;

struct VsIn {
    // A corner of the unit cube, 0 or 1 on each axis.
    @location(0) corner: vec3<f32>,
    // The box this instance stands for.
    @location(1) box_min: vec3<f32>,
    @location(2) box_max: vec3<f32>,
};

@vertex fn vs_main(in: VsIn) -> @builtin(position) vec4<f32> {
    let p = mix(in.box_min, in.box_max, in.corner);
    return globals.view_proj * vec4<f32>(p, 1.0);
}
