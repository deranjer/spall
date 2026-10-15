struct Camera {
    view_projection: mat4x4<f32>,
    effects: vec4<f32>,
};
@group(0) @binding(0) var<uniform> camera: Camera;

struct VertexIn {
    @location(0) position: vec3<f32>,
    @location(1) colour: vec4<f32>,
};
struct VertexOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) colour: vec4<f32>,
};

@vertex
fn vs_snow(input: VertexIn) -> VertexOut {
    var output: VertexOut;
    output.clip = camera.view_projection * vec4<f32>(input.position, 1.0);
    output.colour = input.colour;
    return output;
}

@fragment
fn fs_snow(input: VertexOut) -> @location(0) vec4<f32> {
    // Mild environment-aware shading keeps fresh snow readable without
    // becoming a flat unlit white sheet; color variation stays in the vertices.
    let environment_light = clamp(0.78 + camera.effects.w * 0.2, 0.8, 1.0);
    return vec4<f32>(input.colour.rgb * environment_light, input.colour.a);
}
