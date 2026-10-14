struct SkyGlobals {
    right: vec4<f32>,
    up: vec4<f32>,
    forward: vec4<f32>,
    sun_direction: vec4<f32>,
    moon_direction: vec4<f32>,
    horizon: vec4<f32>,
    zenith: vec4<f32>,
    cloud: vec4<f32>,
    // vertical tan(fov/2), aspect, sun angular radius, sun enabled
    params: vec4<f32>,
    moon_color: vec4<f32>,
    // angular radius, phase (new 0 / full 0.5), visibility, direct strength
    moon_params: vec4<f32>,
};
@group(0) @binding(0) var<uniform> globals: SkyGlobals;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) ndc: vec2<f32>,
};

@vertex fn vs_main(@builtin(vertex_index) index: u32) -> VsOut {
    // Fullscreen triangle, arithmetic form retained for the Vulkan driver fix
    // documented in ENG-60.
    let x = f32(i32(index << 1u) & 2) * 2.0 - 1.0;
    let y = f32(i32(index) & 2) * 2.0 - 1.0;
    var out: VsOut;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.ndc = vec2<f32>(x, y);
    return out;
}

fn hash21(p: vec2<f32>) -> f32 {
    let h = vec3<f32>(p.xyx) * 0.1031;
    return fract((h.x + h.y) * h.z * (h.x + 19.19));
}

// An integer hash of a star cell. The float hash above is fine for the small
// coordinates of the clouds, but at star-field coordinates its precision
// collapses into straight dotted lines of stars.
fn pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

fn star_hash(cell: vec2<i32>) -> u32 {
    return pcg(u32(cell.x) + pcg(u32(cell.y) + 2654435769u));
}

// Stars on a stereographic grid of the sky (about a degree to a cell, evenly
// enough across the upper hemisphere): each cell may hold one star at a fixed
// spot, drawn as a soft disc at least a pixel and a half across, so it slides
// across the screen with the camera instead of flickering in and out of
// pixels. `pixel` is the grid's size of one screen pixel.
fn star_field(p: vec2<f32>, pixel: f32) -> f32 {
    let grid = p * 120.0;
    let cell = floor(grid);
    let h = star_hash(vec2<i32>(cell));
    if (h & 31u) != 0u { return 0.0; }
    let jitter = vec2<f32>(f32((h >> 5u) & 255u), f32((h >> 13u) & 255u)) / 255.0;
    let at = vec2<f32>(0.3) + jitter * 0.4;
    let rank = f32((h >> 21u) & 255u) / 255.0;
    let brightness = 0.3 + 0.7 * rank * rank;
    let d = length(fract(grid) - at);
    let radius = max(pixel * 120.0 * 0.75, 0.04 + 0.06 * brightness);
    // Dim a star that is smaller than its pixels, so its energy is kept.
    let fade = min(1.0, (0.04 + 0.06 * brightness) / radius * 1.6);
    return (1.0 - smoothstep(radius * 0.4, radius, d)) * brightness * fade;
}

fn value_noise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    return mix(mix(hash21(i), hash21(i + vec2<f32>(1.0, 0.0)), u.x),
               mix(hash21(i + vec2<f32>(0.0, 1.0)), hash21(i + vec2<f32>(1.0, 1.0)), u.x), u.y);
}

@fragment fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let ray = normalize(globals.forward.xyz
        + globals.right.xyz * (in.ndc.x * globals.params.y * globals.params.x)
        // ndc comes from clip-space vertex positions, where +Y is up.
        // The framebuffer Y flip happens during rasterization, not here.
        + globals.up.xyz * (in.ndc.y * globals.params.x));
    let elevation = clamp(ray.y * 0.5 + 0.5, 0.0, 1.0);
    // Derivatives are taken here, in uniform control flow, for the stars.
    let star_p = ray.xz / (1.0 + max(ray.y, 0.0));
    let star_pixel = max(length(dpdx(star_p)), length(dpdy(star_p)));
    let horizon_band = exp(-abs(ray.y) * 5.0);
    var color = mix(globals.horizon.rgb, globals.zenith.rgb, smoothstep(-0.08, 0.72, ray.y));
    color += vec3<f32>(0.12, 0.16, 0.2) * horizon_band * 0.22;

    // Broad, voxel-sized sun with a warm square-stepped corona.
    let sun_cos = dot(ray, normalize(globals.sun_direction.xyz));
    let sun_radius = max(globals.params.z, 0.009);
    let sun_edge = cos(sun_radius);
    let sun_distance = acos(clamp(sun_cos, -1.0, 1.0));
    let angular_step = max(sun_radius * 0.13, 0.001);
    let voxel_distance = floor(sun_distance / angular_step) * angular_step;
    let disc = 1.0 - smoothstep(sun_radius * 0.82, sun_radius, voxel_distance);
    let halo = exp(-sun_distance * 38.0) * 0.18;
    color += vec3<f32>(1.0, 0.76, 0.42) * halo;
    if globals.params.w > 0.5 && sun_cos > sun_edge {
        color = mix(color, vec3<f32>(1.0, 0.91, 0.68), disc);
    }

    // Cloud layers live on a camera-independent sky projection. Quantized
    // coordinates and thresholded noise create chunky silhouettes at distance.
    var cloud_cover = 0.0;
    if ray.y > -0.02 {
        let projection = ray.xz / max(ray.y + 0.22, 0.18);
        let cloud_uv = floor(projection * 13.0) / 13.0;
        let broad = value_noise(cloud_uv * 1.9 + vec2<f32>(7.3, 2.1));
        let detail = value_noise(cloud_uv * 5.7 + vec2<f32>(1.7, 9.2));
        let density = broad * 0.76 + detail * 0.24;
        let horizon_fade = smoothstep(-0.03, 0.14, ray.y);
        let cloud_alpha = smoothstep(0.57, 0.72, density) * horizon_fade * 0.88;
        let lighting = 0.78 + max(dot(ray, normalize(globals.sun_direction.xyz)), 0.0) * 0.22;
        color = mix(color, globals.cloud.rgb * lighting, cloud_alpha);
        cloud_cover = cloud_alpha;
    }
    // Keep the sun readable over the procedural cloud silhouette.
    color = mix(color, vec3<f32>(1.0, 0.91, 0.68), disc * globals.params.w);

    // The moon follows an orbit offset by its lunar age, and a soft terminator
    // reveals the waxing/waning phase. The body-facing portion points toward
    // the sun; daytime visibility is attenuated by moon_params.z in the CPU
    // environment builder.
    let moon_direction = normalize(globals.moon_direction.xyz);
    let moon_cos = dot(ray, moon_direction);
    let moon_distance = acos(clamp(moon_cos, -1.0, 1.0));
    let moon_radius = max(globals.moon_params.x, 0.006);
    let moon_disc = 1.0 - smoothstep(moon_radius * 0.88, moon_radius, moon_distance);
    let moon_position = moon_direction;
    let sun_position = normalize(globals.sun_direction.xyz);
    let to_sun = normalize(sun_position - moon_position * dot(sun_position, moon_position));
    let tangent = normalize(ray - moon_position * dot(ray, moon_position));
    let sunward = dot(tangent, to_sun);
    let illumination = 0.5 - 0.5 * cos(globals.moon_params.y * 6.2831853);
    let terminator = cos(illumination * 3.14159265);
    let phase_light = smoothstep(terminator - 0.035, terminator + 0.035, sunward);
    let lunar_surface = mix(vec3<f32>(0.09, 0.12, 0.2), globals.moon_color.rgb, phase_light);
    color = mix(color, lunar_surface, moon_disc * globals.moon_params.z);

    // A small stable star field comes out as daylight fades. Stars use the
    // same sky projection and remain fixed as the camera moves.
    if globals.params.w < 0.5 && ray.y > 0.04 {
        let star = star_field(star_p, star_pixel);
        // Clouds hide the stars behind them.
        color += vec3<f32>(0.52, 0.62, 0.82) * star * 1.4 * (1.0 - cloud_cover);
    }
    // Keep the output in a useful HDR range for the shared ACES tone mapper.
    let upper_sky = 0.92 + 0.08 * elevation;
    return vec4<f32>(color * upper_sky, 1.0);
}
