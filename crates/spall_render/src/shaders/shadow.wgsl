struct ShadowGlobals { light_view_proj: mat4x4<f32>, };
@group(0) @binding(0) var<uniform> globals: ShadowGlobals;
struct VsIn { @location(0) position: vec3<f32>, };
@vertex fn vs_main(in: VsIn) -> @builtin(position) vec4<f32> {
    return globals.light_view_proj * vec4<f32>(in.position, 1.0);
}
struct CubeIn {
    @location(0) position: vec3<f32>,
    @location(3) offset: vec3<f32>,
    @location(5) size: vec3<f32>,
    @location(6) rotation: vec4<f32>,
};
fn quat_rotate(q: vec4<f32>, v: vec3<f32>) -> vec3<f32> {
    let t = 2.0 * cross(q.xyz, v);
    return v + q.w * t + cross(q.xyz, t);
}
@vertex fn vs_cube(in: CubeIn) -> @builtin(position) vec4<f32> {
    let world = quat_rotate(in.rotation, in.position * in.size) + in.offset;
    return globals.light_view_proj * vec4<f32>(world, 1.0);
}
