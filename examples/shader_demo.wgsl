/// Example WebGPU shader showcasing TrackerTop WGSL extraction (structs, bindings, entry points, calls).
struct FrameUniforms {
    resolution: vec2<f32>,
    time: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> frame: FrameUniforms;

fn palette_glow(t: f32) -> vec3<f32> {
    return vec3<f32>(0.22 + 0.3 * sin(t), 0.74, 0.97);
}

@vertex
fn vs_fullscreen(@builtin(vertex_index) idx: u32) -> @builtin(position) vec4<f32> {
    var pos = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(pos[idx], 0.0, 1.0);
}

@fragment
fn fs_visualize(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = frag_coord.xy / max(frame.resolution, vec2<f32>(1.0, 1.0));
    let col = palette_glow(frame.time + uv.x * 3.14159);
    return vec4<f32>(col, 1.0);
}
