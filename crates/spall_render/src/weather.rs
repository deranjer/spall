//! Presentation-only weather primitives consumed by [`crate::GameRenderer`].

/// Absolute renderer input cap; the client policy uses the same bound.
pub const MAX_WEATHER_PARTICLES: usize = 12_288;

/// One cosmetic, camera-local weather ribbon in world metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeatherParticle {
    pub centre: [f32; 3],
    /// Unit-ish world direction. Renderer turns this into a camera-facing ribbon.
    pub direction: [f32; 3],
    pub length_m: f32,
    pub width_m: f32,
    pub colour_rgba: [f32; 4],
}

/// Weather values available for host sky, audio and visibility presentation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeatherLighting {
    /// Fraction of the camera-local sky grid with known unobstructed sky.
    pub exposure: f32,
    pub precipitation: f32,
    pub wind: f32,
    /// 1 is clear visibility; a blizzard whiteout pulls it down toward 0.15.
    pub visibility: f32,
    /// Storm darkening amount for the host's sky/environment presentation.
    pub storm_dimming: f32,
    /// Bounded, smooth lightning contribution; zero when reduced flashes are on.
    pub lightning_flash: f32,
}

impl Default for WeatherLighting {
    fn default() -> Self {
        Self {
            exposure: 1.0,
            precipitation: 0.0,
            wind: 0.0,
            visibility: 1.0,
            storm_dimming: 0.0,
            lightning_flash: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WeatherRenderError {
    #[error("weather particle count exceeds the renderer bound")]
    TooManyParticles,
    #[error("weather particle contains a non-finite value")]
    NonFiniteParticle,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct WeatherVertex {
    pub position: [f32; 3],
    pub colour: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct WeatherUniform {
    pub view_projection: [f32; 16],
    pub effects: [f32; 4],
}

impl WeatherVertex {
    pub(crate) const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4],
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn weather_vertex_and_overlay_wgsl_validate() {
        let source = include_str!("shaders/weather.wgsl");
        let module = naga::front::wgsl::parse_str(source).expect("weather WGSL parses");
        naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("weather shader entry points validate");
    }
}
