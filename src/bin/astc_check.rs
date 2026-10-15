// muguet - astc_check.rs
// Constantin Clerc (c22dev) - 0.1

use wgpu::util::DeviceExt;
use wgpu::*;

const BW: u32 = 64;
const BH: u32 = 64;

fn expand(c: u32) -> [i32; 3] {
    let (r, g, b) = ((c >> 11) & 31, (c >> 5) & 63, c & 31);
    [((r << 3) | (r >> 2)) as i32, ((g << 2) | (g >> 4)) as i32, ((b << 3) | (b >> 2)) as i32]
}

fn bc1(w0: u32, idx: u32, k: u32) -> [i32; 4] {
    let (c0, c1) = (w0 & 0xffff, w0 >> 16);
    let (a, b) = (expand(c0), expand(c1));
    let id = (idx >> (2 * k)) & 3;
    let mix = |p: i32, q: i32| [(a[0] * p + b[0] * q) / (p + q), (a[1] * p + b[1] * q) / (p + q), (a[2] * p + b[2] * q) / (p + q)];
    let rgb = if c0 > c1 {
        match id { 0 => a, 1 => b, 2 => mix(2, 1), _ => mix(1, 2) }
    } else {
        match id { 0 => a, 1 => b, 2 => mix(1, 1), _ => return [0, 0, 0, 0] }
    };
    [rgb[0], rgb[1], rgb[2], 255]
}

fn main() {
    let instance = Instance::new(InstanceDescriptor { backends: Backends::METAL, ..InstanceDescriptor::new_without_display_handle() });
    let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions::default())).expect("adapter");
    assert!(adapter.features().contains(Features::TEXTURE_COMPRESSION_ASTC), "this GPU has no ASTC");
    let l = adapter.limits();
    println!("limits: max_buffer_size {} MB, max_storage_buffer_binding_size {} MB, max_compute_workgroups_per_dimension {}", l.max_buffer_size >> 20, l.max_storage_buffer_binding_size >> 20, l.max_compute_workgroups_per_dimension);
    let (device, queue) = pollster::block_on(adapter.request_device(&DeviceDescriptor { required_features: Features::TEXTURE_COMPRESSION_ASTC, ..Default::default() })).expect("device");

    let mut seed = 0x2545F4914F6CDD1Du64;
    let mut rnd = || { seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17; (seed >> 16) as u32 };
    let mut src = Vec::new();
    for i in 0..BW * BH {
        let (mut c0, mut c1) = (rnd() & 0xffff, rnd() & 0xffff);
        let mut idx = rnd();
        match i % 3 {
            0 => { if c0 <= c1 { std::mem::swap(&mut c0, &mut c1); } if c0 == c1 { c0 |= 1; c1 &= !1; } }
            1 => { if c0 > c1 { std::mem::swap(&mut c0, &mut c1); } for k in 0..16 { if (idx >> (2 * k)) & 3 == 3 { idx &= !(1 << (2 * k)); } } }
            _ => { if c0 > c1 { std::mem::swap(&mut c0, &mut c1); } idx |= 3 << (2 * (rnd() % 16)); }
        }
        src.push(c0 | (c1 << 16));
        src.push(idx);
    }

    let src_buf = device.create_buffer_init(&util::BufferInitDescriptor { label: None, contents: bytemuck_u32(&src), usage: BufferUsages::STORAGE });
    let row_bytes = (BW * 16).div_ceil(256) * 256;
    let dst_buf = device.create_buffer(&BufferDescriptor { label: None, size: (row_bytes * BH) as u64, usage: BufferUsages::STORAGE | BufferUsages::COPY_SRC, mapped_at_creation: false });
    let mut jobs: Vec<u32> = Vec::new();
    for j in 0..4u32 {
        let rows = BH / 4;
        jobs.extend_from_slice(&[j * BW * rows, BW, rows, 1, j * BW * rows * 2, j * rows * (row_bytes / 4), row_bytes / 4, 0]);
    }
    let par_buf = device.create_buffer_init(&util::BufferInitDescriptor { label: None, contents: bytemuck_u32(&jobs), usage: BufferUsages::STORAGE });
    let module = device.create_shader_module(ShaderModuleDescriptor { label: Some("bc1_astc"), source: ShaderSource::Wgsl(include_str!("../bc1_astc.wgsl").into()) });
    let pipe = device.create_compute_pipeline(&ComputePipelineDescriptor { label: None, layout: None, module: &module, entry_point: Some("main"), compilation_options: Default::default(), cache: None });
    let bg = device.create_bind_group(&BindGroupDescriptor { label: None, layout: &pipe.get_bind_group_layout(0), entries: &[
        BindGroupEntry { binding: 0, resource: src_buf.as_entire_binding() },
        BindGroupEntry { binding: 1, resource: dst_buf.as_entire_binding() },
        BindGroupEntry { binding: 2, resource: par_buf.as_entire_binding() },
    ] });

    let astc = device.create_texture(&TextureDescriptor {
        label: None, size: Extent3d { width: BW * 4, height: BH * 4, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1,
        dimension: TextureDimension::D2, format: TextureFormat::Astc { block: AstcBlock::B4x4, channel: AstcChannel::Unorm },
        usage: TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING, view_formats: &[],
    });
    let out = device.create_texture(&TextureDescriptor {
        label: None, size: Extent3d { width: BW * 4, height: BH * 4, depth_or_array_layers: 1 }, mip_level_count: 1, sample_count: 1,
        dimension: TextureDimension::D2, format: TextureFormat::Rgba8Unorm, usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC, view_formats: &[],
    });
    let draw = device.create_shader_module(ShaderModuleDescriptor { label: None, source: ShaderSource::Wgsl(
        "@group(0) @binding(0) var t: texture_2d<f32>;
         @vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f { var p = array<vec2f, 3>(vec2f(-1, -1), vec2f(3, -1), vec2f(-1, 3)); return vec4f(p[i], 0, 1); }
         @fragment fn fs(@builtin(position) pos: vec4f) -> @location(0) vec4f { return textureLoad(t, vec2i(pos.xy), 0); }".into()) });
    let rp = device.create_render_pipeline(&RenderPipelineDescriptor {
        label: None, layout: None,
        vertex: VertexState { module: &draw, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
        primitive: PrimitiveState::default(), depth_stencil: None, multisample: MultisampleState::default(),
        fragment: Some(FragmentState { module: &draw, entry_point: Some("fs"), compilation_options: Default::default(), targets: &[Some(ColorTargetState { format: TextureFormat::Rgba8Unorm, blend: None, write_mask: ColorWrites::ALL })] }),
        multiview_mask: None, cache: None,
    });
    let astc_view = astc.create_view(&TextureViewDescriptor::default());
    let rbg = device.create_bind_group(&BindGroupDescriptor { label: None, layout: &rp.get_bind_group_layout(0), entries: &[BindGroupEntry { binding: 0, resource: BindingResource::TextureView(&astc_view) }] });
    let read_row = BW * 4 * 4;
    let read = device.create_buffer(&BufferDescriptor { label: None, size: (read_row * BH * 4) as u64, usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ, mapped_at_creation: false });

    let mut enc = device.create_command_encoder(&CommandEncoderDescriptor::default());
    {
        let mut c = enc.begin_compute_pass(&ComputePassDescriptor::default());
        c.set_pipeline(&pipe);
        c.set_bind_group(0, &bg, &[]);
        c.dispatch_workgroups((BW * BH).div_ceil(64), 1, 1);
    }
    enc.copy_buffer_to_texture(
        TexelCopyBufferInfo { buffer: &dst_buf, layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row_bytes), rows_per_image: Some(BH) } },
        TexelCopyTextureInfo { texture: &astc, mip_level: 0, origin: Origin3d::ZERO, aspect: TextureAspect::All },
        Extent3d { width: BW * 4, height: BH * 4, depth_or_array_layers: 1 },
    );
    {
        let ov = out.create_view(&TextureViewDescriptor::default());
        let mut p = enc.begin_render_pass(&RenderPassDescriptor {
            label: None, color_attachments: &[Some(RenderPassColorAttachment { view: &ov, depth_slice: None, resolve_target: None, ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store } })],
            depth_stencil_attachment: None, timestamp_writes: None, occlusion_query_set: None, multiview_mask: None,
        });
        p.set_pipeline(&rp);
        p.set_bind_group(0, &rbg, &[]);
        p.draw(0..3, 0..1);
    }
    enc.copy_texture_to_buffer(
        TexelCopyTextureInfo { texture: &out, mip_level: 0, origin: Origin3d::ZERO, aspect: TextureAspect::All },
        TexelCopyBufferInfo { buffer: &read, layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(read_row), rows_per_image: Some(BH * 4) } },
        Extent3d { width: BW * 4, height: BH * 4, depth_or_array_layers: 1 },
    );
    queue.submit([enc.finish()]);
    read.slice(..).map_async(MapMode::Read, |r| r.expect("map"));
    device.poll(PollType::Wait { submission_index: None, timeout: None }).expect("poll");
    let px = read.slice(..).get_mapped_range().expect("mapped").to_vec();

    let names = ["4-colour", "3-colour opaque", "3-colour + transparent"];
    let mut worst = [0i32; 3];
    let mut worst_mid = [0i32; 3];
    let mut alpha_bad = [0u32; 3];
    for by in 0..BH {
        for bx in 0..BW {
            let bi = (by * BW + bx) as usize;
            let (w0, idx) = (src[bi * 2], src[bi * 2 + 1]);
            let kind = bi % 3;
            for k in 0..16u32 {
                let (tx, ty) = (bx * 4 + k % 4, by * 4 + k / 4);
                let o = (ty * read_row + tx * 4) as usize;
                let got = [px[o] as i32, px[o + 1] as i32, px[o + 2] as i32, px[o + 3] as i32];
                let want = bc1(w0, idx, k);
                let id = (idx >> (2 * k)) & 3;
                if (got[3] > 127) != (want[3] > 127) { alpha_bad[kind] += 1; continue; }
                if want[3] == 0 { continue; }
                let err = (0..3).map(|c| (got[c] - want[c]).abs()).max().unwrap();
                let approximated = kind != 0 && id == 2;
                if approximated { worst_mid[kind] = worst_mid[kind].max(err); } else { worst[kind] = worst[kind].max(err); }
            }
        }
    }
    let mut ok = true;
    for k in 0..3 {
        println!("{:24} max error {:3}/255 on exact texels, {:3}/255 on midpoint texels, {} alpha mismatches", names[k], worst[k], worst_mid[k], alpha_bad[k]);
        if worst[k] > 3 || alpha_bad[k] != 0 { ok = false; }
    }
    println!("{}", if ok { "PASS" } else { "FAIL" });

    let lost = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let l2 = lost.clone();
    device.set_device_lost_callback(move |r, m| { println!("device lost: {r:?} {m}"); l2.store(true, std::sync::atomic::Ordering::SeqCst); });
    let small = device.create_texture(&TextureDescriptor {
        label: None, size: Extent3d { width: 8, height: 8, depth_or_array_layers: 1 }, mip_level_count: 4, sample_count: 1,
        dimension: TextureDimension::D2, format: TextureFormat::Astc { block: AstcBlock::B4x4, channel: AstcChannel::Unorm },
        usage: TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING, view_formats: &[],
    });
    let one = device.create_buffer(&BufferDescriptor { label: None, size: 256 * 2, usage: BufferUsages::COPY_DST | BufferUsages::COPY_SRC, mapped_at_creation: false });
    for (mip, n) in [(0u32, 2u32), (1, 1), (2, 1), (3, 1)] {
        let mut e = device.create_command_encoder(&CommandEncoderDescriptor::default());
        e.copy_buffer_to_texture(
            TexelCopyBufferInfo { buffer: &one, layout: TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(n) } },
            TexelCopyTextureInfo { texture: &small, mip_level: mip, origin: Origin3d::ZERO, aspect: TextureAspect::All },
            Extent3d { width: n * 4, height: n * 4, depth_or_array_layers: 1 },
        );
        queue.submit([e.finish()]);
        device.poll(PollType::Wait { submission_index: None, timeout: None }).ok();
        println!("mip {mip} ({}x{}) copy of {}x{} texels: {}", (8 >> mip).max(1), (8 >> mip).max(1), n * 4, n * 4, if lost.load(std::sync::atomic::Ordering::SeqCst) { "DEVICE LOST" } else { "ok" });
    }

    let count = 20000u32;
    let mut srcs: Vec<u32> = Vec::new();
    let mut js: Vec<u32> = Vec::new();
    for k in 0..count {
        srcs.extend_from_slice(&src[..32]);
        js.extend_from_slice(&[k * 16, 4, 4, 1, k * 32, k * 64 * 4, 64, 0]);
    }
    let s2 = device.create_buffer_init(&util::BufferInitDescriptor { label: None, contents: bytemuck_u32(&srcs), usage: BufferUsages::STORAGE });
    let d2 = device.create_buffer(&BufferDescriptor { label: None, size: (count * 256 * 4) as u64, usage: BufferUsages::STORAGE | BufferUsages::COPY_SRC, mapped_at_creation: false });
    let j2 = device.create_buffer_init(&util::BufferInitDescriptor { label: None, contents: bytemuck_u32(&js), usage: BufferUsages::STORAGE });
    let g2 = device.create_bind_group(&BindGroupDescriptor { label: None, layout: &pipe.get_bind_group_layout(0), entries: &[
        BindGroupEntry { binding: 0, resource: s2.as_entire_binding() },
        BindGroupEntry { binding: 1, resource: d2.as_entire_binding() },
        BindGroupEntry { binding: 2, resource: j2.as_entire_binding() },
    ] });
    let mut e = device.create_command_encoder(&CommandEncoderDescriptor::default());
    {
        let mut c = e.begin_compute_pass(&ComputePassDescriptor::default());
        c.set_pipeline(&pipe);
        c.set_bind_group(0, &g2, &[]);
        c.dispatch_workgroups((count * 16).div_ceil(64), 1, 1);
    }
    queue.submit([e.finish()]);
    device.poll(PollType::Wait { submission_index: None, timeout: None }).ok();
    println!("{count} uploads in one batched dispatch: {}", if lost.load(std::sync::atomic::Ordering::SeqCst) { "DEVICE LOST" } else { "ok" });
}

fn bytemuck_u32(v: &[u32]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 4) }
}
