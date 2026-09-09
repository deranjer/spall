struct ShadowGlobals { light_view_proj: mat4x4<f32>, };
@group(0) @binding(0) var<uniform> globals: ShadowGlobals;
struct VsIn { @location(0) position: vec3<f32>, };
@vertex fn vs_main(in: VsIn) -> @builtin(position) vec4<f32> {
    return globals.light_view_proj * vec4<f32>(in.position, 1.0);
}
