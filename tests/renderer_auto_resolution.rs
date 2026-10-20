mod common;

use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::process::Command;
use std::time::Duration;
use waywallen::wallframe::dma::negotiate as dma;
use waywallen::wallframe::ipc::proto::*;
use waywallen::wallframe::ipc::uds::{recv_event, send_control};
use waywallen::wallframe::sync::DrmDevice;

nix::ioctl_write_ptr!(dma_buf_sync, b'b', 0, u64);

fn pixel_hash(pool: &BufferPool, fd: &OwnedFd) -> u64 {
    assert_eq!(pool.format.modifier, 0);
    assert_eq!(pool.format.plane_count, 1);
    let size = usize::try_from(pool.size[0]).unwrap();
    let offset = pool.plane_offset[0] as usize;
    let stride = pool.stride[0] as usize;
    let row_bytes = pool.extent.width as usize * 4;
    assert!(offset + stride * (pool.extent.height as usize - 1) + row_bytes <= size);
    unsafe {
        let ptr = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        );
        assert_ne!(ptr, libc::MAP_FAILED);
        dma_buf_sync(fd.as_raw_fd(), &1).unwrap();
        let data = std::slice::from_raw_parts(ptr.cast::<u8>(), size);
        let mut hash = 0xcbf29ce484222325_u64;
        for row in 0..pool.extent.height as usize {
            for byte in &data[offset + row * stride..offset + row * stride + row_bytes] {
                hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
            }
        }
        dma_buf_sync(fd.as_raw_fd(), &5).unwrap();
        assert_eq!(libc::munmap(ptr, size), 0);
        hash
    }
}

#[test]
fn paused_renderer_resizes_without_restarting() {
    let Ok(binary) = std::env::var("WAYWALLEN_AUTO_RENDERER_BIN") else {
        eprintln!(
            "skip: set WAYWALLEN_AUTO_RENDERER_BIN and WAYWALLEN_AUTO_MEDIA_PATH for the GPU test"
        );
        return;
    };
    let media = std::env::var("WAYWALLEN_AUTO_MEDIA_PATH").expect("media path");
    let drm = DrmDevice::open_first_render_node().expect("DRM device");
    let socket = common::tmp_sock("auto-resolution");
    let _cleanup = common::SockCleanup(socket.clone());
    let listener = UnixListener::bind(&socket).unwrap();
    let mut command = Command::new(binary);
    command.args(["--ipc", socket.to_str().unwrap(), "--path", &media]);
    if let Ok(assets) = std::env::var("WAYWALLEN_AUTO_ASSETS_PATH") {
        command.args(["--assets", &assets]);
    }
    let mut child = common::ChildGuard(command.spawn().unwrap());
    let (stream, _) = common::accept_with_timeout(&listener, Duration::from_secs(10))
        .unwrap()
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    send_control(
        &stream,
        &ControlMsg::Init {
            config: RendererInit {
                protocol_version: PROTOCOL_VERSION,
                spawn_version: waywallen::wallframe::renderer_manager::SPAWN_VERSION,
                settings: vec![
                    ("resolution".into(), "-2".into()),
                    ("enable_audio".into(), "false".into()),
                    (
                        "hwdec".into(),
                        std::env::var("WAYWALLEN_AUTO_HWDEC").unwrap_or("none".into()),
                    ),
                ],
                user_properties: String::new(),
            },
        },
        &[],
    )
    .unwrap();
    let mut timeline = None;
    let caps = loop {
        let (event, mut fds) = recv_event(&stream).unwrap();
        match event {
            EventMsg::ReleaseSyncobj => timeline = Some(drm.fd_to_handle(&fds.remove(0)).unwrap()),
            EventMsg::RenderTargetCapabilities { capabilities } => break capabilities,
            EventMsg::BindBuffers { .. } => panic!("allocated before receiving the first target"),
            EventMsg::Error { message } => panic!("{message}"),
            _ => {}
        }
    };
    send_control(
        &stream,
        &ControlMsg::Pause {
            transition: ControlTransition { fade_ms: 0 },
        },
        &[],
    )
    .unwrap();
    let mut old_buffers = Vec::new();
    let mut pools = Vec::new();
    let mut hashes = Vec::new();
    let mut fd_counts = Vec::new();
    let mut generation = 0;
    let mut sequence = 0;
    for (index, divisor) in [2, 1, 4, 2].into_iter().enumerate() {
        let scale = if caps.mode == 2
            && index == 1
            && caps.reference_width <= caps.max_width / 2
            && caps.reference_height <= caps.max_height / 2
        {
            2
        } else {
            1
        };
        let width = (caps.reference_width * scale / divisor / caps.width_alignment
            * caps.width_alignment)
            .max(caps.min_width);
        let height = (caps.reference_height * scale / divisor / caps.height_alignment
            * caps.height_alignment)
            .max(caps.min_height);
        let config = RenderTargetConfig {
            revision: index as u64 + 1,
            capabilities_revision: caps.revision,
            width,
            height,
            fourcc: dma::DRM_FORMAT_ABGR8888,
            modifier: 0,
            plane_count: 1,
            sync_mode: dma::SYNC_SYNCOBJ_TIMELINE,
            color: dma::DEFAULT_COLOR,
            mem_hint: dma::MEM_HINT_HOST_VISIBLE,
            count: 1,
            path: BufferPath::CompatLinear,
            memory_source: BufferMemorySource::GpuLinear,
        };
        if index == 1 {
            let mut invalid = config.clone();
            invalid.revision = 99;
            invalid.width = caps.reference_width + caps.width_alignment;
            send_control(
                &stream,
                &ControlMsg::ConfigureRenderTarget { config: invalid },
                &[],
            )
            .unwrap();
            loop {
                match recv_event(&stream).unwrap().0 {
                    EventMsg::RenderTargetResult { result } => {
                        assert_eq!(result.revision, 99);
                        assert_eq!(result.status, 1);
                        break;
                    }
                    EventMsg::BindBuffers { .. } | EventMsg::FrameReady { .. } => {
                        panic!("invalid target changed the active pool")
                    }
                    _ => {}
                }
            }
        }
        send_control(
            &stream,
            &ControlMsg::ConfigureRenderTarget {
                config: config.clone(),
            },
            &[],
        )
        .unwrap();
        let mut frame_seen = false;
        let mut result_seen = false;
        loop {
            let (event, fds) = recv_event(&stream).unwrap();
            match event {
                EventMsg::BindBuffers { pool } => {
                    assert!(pool.generation > generation);
                    generation = pool.generation;
                    assert_eq!((pool.extent.width, pool.extent.height), (width, height));
                    pools.push(pool);
                    old_buffers.extend(fds);
                }
                EventMsg::FrameReady { frame } => {
                    assert_eq!(pools.len(), index + 1, "frame arrived before the new pool");
                    assert!(!frame_seen, "paused renderer published multiple frames");
                    frame_seen = true;
                    assert!(frame.sequence > sequence);
                    sequence = frame.sequence;
                    let acquire = drm.create_binary_syncobj().unwrap();
                    drm.import_sync_file(&acquire, &fds[0]).unwrap();
                    let mut fence = libc::pollfd {
                        fd: fds[0].as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    assert_eq!(unsafe { libc::poll(&mut fence, 1, 10_000) }, 1);
                    hashes.push(pixel_hash(
                        pools.last().unwrap(),
                        old_buffers.last().unwrap(),
                    ));
                    drm.transfer(&acquire, 0, timeline.as_ref().unwrap(), frame.release_point)
                        .unwrap();
                }
                EventMsg::RenderTargetResult { result } => {
                    assert_eq!(result.revision, config.revision);
                    assert_eq!(result.status, 0, "{}", result.message);
                    assert_eq!(result.pool_generation, generation);
                    result_seen = true;
                }
                EventMsg::Error { message } => panic!("{message}"),
                _ => {}
            }
            if frame_seen && result_seen {
                break;
            }
        }
        assert!(child.0.try_wait().unwrap().is_none());
        fd_counts.push(
            std::fs::read_dir(format!("/proc/{}/fd", child.0.id()))
                .unwrap()
                .count(),
        );
        eprintln!(
            "Auto revision={} extent={}x{} generation={} sequence={sequence}",
            config.revision, width, height, generation
        );
    }
    assert_eq!(
        old_buffers.len(),
        4,
        "old exports remain alive across resize"
    );
    assert_eq!(
        hashes[0], hashes[3],
        "resize must not advance paused content"
    );
    for ((pool, fd), expected) in pools.iter().zip(&old_buffers).zip(&hashes) {
        assert_eq!(
            pixel_hash(pool, fd),
            *expected,
            "retired backing was overwritten"
        );
    }
    assert!(
        fd_counts[3] <= fd_counts[0] + 2,
        "renderer leaked FDs: {fd_counts:?}"
    );
    send_control(&stream, &ControlMsg::Shutdown, &[]).unwrap();
}
