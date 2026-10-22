struct Camera {
    view_projection: mat4x4<f32>,
    effects: vec4<f32>, // visibility, storm dimming, lightning, exposure
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
fn vs_weather(input: VertexIn) -> VertexOut {
    var output: VertexOut;
    output.clip = camera.view_projection * vec4<f32>(input.position, 1.0);
    output.colour = input.colour;
    return output;
}

@fragment
fn fs_weather(input: VertexOut) -> @location(0) vec4<f32> {
    return input.colour;
}

@vertex
fn vs_overlay(@builtin(vertex_index) index: u32) -> @builtin(position) vec4<f32> {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(positions[index], 0.0, 1.0);
}

@fragment
fn fs_overlay() -> @location(0) vec4<f32> {
    let fog = clamp(1.0 - camera.effects.x, 0.0, 1.0);
    let dim = clamp(camera.effects.y, 0.0, 0.22);
    let flash = clamp(camera.effects.z, 0.0, 0.08);
    // Bright grey-white scaled by the scene's brightness; distance fog does most of the whiteout.
    let white = vec3<f32>(0.9, 0.95, 1.0) * camera.effects.w * 1.15;
    let fog_colour = mix(vec3<f32>(0.08, 0.11, 0.15), white, clamp(fog * 2.0, 0.0, 1.0));
    let colour = fog_colour * (1.0 - dim) + vec3<f32>(flash * 2.0);
    let alpha = clamp(fog * 0.3 + dim + flash, 0.0, 0.36);
    return vec4<f32>(colour, alpha);
}
