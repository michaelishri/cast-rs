use anyhow::{Context, Result, anyhow, bail};
use screencapturekit::{
    IOSurface,
    cm::{IOSurfaceLockOptions, PlaneProperties},
};

const PIXEL_FORMAT_420V: u32 = u32::from_be_bytes(*b"420v");
const ROW_ALIGNMENT: usize = 64;
pub(crate) const SYNTHETIC_CYCLE_SECONDS: u64 = 10;
pub(crate) const SYNTHETIC_WORKLOAD_NAME: &str = "synthetic-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SyntheticPhase {
    Static,
    PartialMotion,
    FullMotion,
    SceneCuts,
}

impl SyntheticPhase {
    pub(crate) const ALL: [Self; 4] = [
        Self::Static,
        Self::PartialMotion,
        Self::FullMotion,
        Self::SceneCuts,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::PartialMotion => "partial motion",
            Self::FullMotion => "full motion",
            Self::SceneCuts => "scene cuts",
        }
    }

    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Static => 0,
            Self::PartialMotion => 1,
            Self::FullMotion => 2,
            Self::SceneCuts => 3,
        }
    }
}

pub(crate) fn phase_for_frame(frame_index: u64, fps: u32) -> (SyntheticPhase, u64) {
    let cycle_frames = u64::from(fps.max(1)).saturating_mul(SYNTHETIC_CYCLE_SECONDS);
    let position = frame_index % cycle_frames;
    let phase_index = position.saturating_mul(4) / cycle_frames;
    let phase_start = phase_index.saturating_mul(cycle_frames) / 4;
    let phase = match phase_index {
        0 => SyntheticPhase::Static,
        1 => SyntheticPhase::PartialMotion,
        2 => SyntheticPhase::FullMotion,
        _ => SyntheticPhase::SceneCuts,
    };
    (phase, position.saturating_sub(phase_start))
}

pub(crate) struct SyntheticFrameGenerator {
    surface: IOSurface,
    width: usize,
    height: usize,
    fps: u32,
    y_bytes_per_row: usize,
    uv_bytes_per_row: usize,
    y_size: usize,
    uv_size: usize,
    base_y: Vec<u8>,
    base_uv: Vec<u8>,
}

impl SyntheticFrameGenerator {
    pub(crate) fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        if width < 2 || height < 2 || width % 2 != 0 || height % 2 != 0 {
            bail!("synthetic frame dimensions must be even and at least 2x2");
        }
        if fps == 0 {
            bail!("synthetic frame rate must be greater than zero");
        }

        let width = usize::try_from(width).context("synthetic width exceeded usize")?;
        let height = usize::try_from(height).context("synthetic height exceeded usize")?;
        let y_bytes_per_row = align_up(width, ROW_ALIGNMENT)
            .ok_or_else(|| anyhow!("synthetic luma row size overflowed"))?;
        let uv_bytes_per_row = y_bytes_per_row;
        let y_size = y_bytes_per_row
            .checked_mul(height)
            .ok_or_else(|| anyhow!("synthetic luma allocation overflowed"))?;
        let uv_size = uv_bytes_per_row
            .checked_mul(height / 2)
            .ok_or_else(|| anyhow!("synthetic chroma allocation overflowed"))?;
        let allocation_size = y_size
            .checked_add(uv_size)
            .ok_or_else(|| anyhow!("synthetic IOSurface allocation overflowed"))?;
        let planes = [
            PlaneProperties {
                width,
                height,
                bytes_per_row: y_bytes_per_row,
                bytes_per_element: 1,
                offset: 0,
                size: y_size,
            },
            PlaneProperties {
                width: width / 2,
                height: height / 2,
                bytes_per_row: uv_bytes_per_row,
                bytes_per_element: 2,
                offset: y_size,
                size: uv_size,
            },
        ];
        let surface = IOSurface::create_with_properties(
            width,
            height,
            PIXEL_FORMAT_420V,
            1,
            y_bytes_per_row,
            allocation_size,
            Some(&planes),
        )
        .ok_or_else(|| anyhow!("could not allocate a synthetic 420v IOSurface"))?;
        if surface.plane_count() != 2 {
            bail!(
                "synthetic IOSurface has {} planes instead of two",
                surface.plane_count()
            );
        }

        let (base_y, base_uv) =
            build_static_frame(width, height, y_bytes_per_row, uv_bytes_per_row);
        let mut generator = Self {
            surface,
            width,
            height,
            fps,
            y_bytes_per_row,
            uv_bytes_per_row,
            y_size,
            uv_size,
            base_y,
            base_uv,
        };
        generator.render(0)?;
        Ok(generator)
    }

    pub(crate) const fn surface(&self) -> &IOSurface {
        &self.surface
    }

    pub(crate) fn render(&mut self, frame_index: u64) -> Result<SyntheticPhase> {
        let (phase, phase_frame) = phase_for_frame(frame_index, self.fps);
        let mut guard = self
            .surface
            .lock(IOSurfaceLockOptions::NONE)
            .map_err(|status| anyhow!("could not lock synthetic IOSurface: {status}"))?;
        let allocation = guard
            .as_slice_mut()
            .ok_or_else(|| anyhow!("synthetic IOSurface was unexpectedly read-only"))?;
        if allocation.len() < self.y_size + self.uv_size {
            bail!(
                "synthetic IOSurface exposes {} bytes, expected at least {}",
                allocation.len(),
                self.y_size + self.uv_size
            );
        }
        let (y_plane, remainder) = allocation.split_at_mut(self.y_size);
        let uv_plane = &mut remainder[..self.uv_size];
        y_plane.copy_from_slice(&self.base_y);
        uv_plane.copy_from_slice(&self.base_uv);

        match phase {
            SyntheticPhase::Static => {}
            SyntheticPhase::PartialMotion => render_partial_motion(
                y_plane,
                uv_plane,
                PlaneLayout {
                    width: self.width,
                    height: self.height,
                    y_bytes_per_row: self.y_bytes_per_row,
                    uv_bytes_per_row: self.uv_bytes_per_row,
                },
                phase_frame,
            ),
            SyntheticPhase::FullMotion => render_full_motion(
                y_plane,
                uv_plane,
                PlaneLayout {
                    width: self.width,
                    height: self.height,
                    y_bytes_per_row: self.y_bytes_per_row,
                    uv_bytes_per_row: self.uv_bytes_per_row,
                },
                frame_index,
            ),
            SyntheticPhase::SceneCuts => render_scene_cut(
                y_plane,
                uv_plane,
                PlaneLayout {
                    width: self.width,
                    height: self.height,
                    y_bytes_per_row: self.y_bytes_per_row,
                    uv_bytes_per_row: self.uv_bytes_per_row,
                },
                phase_frame,
                self.fps,
            ),
        }
        Ok(phase)
    }
}

#[derive(Clone, Copy)]
struct PlaneLayout {
    width: usize,
    height: usize,
    y_bytes_per_row: usize,
    uv_bytes_per_row: usize,
}

fn build_static_frame(
    width: usize,
    height: usize,
    y_bytes_per_row: usize,
    uv_bytes_per_row: usize,
) -> (Vec<u8>, Vec<u8>) {
    let mut y_plane = vec![16_u8; y_bytes_per_row * height];
    let mut uv_plane = vec![128_u8; uv_bytes_per_row * (height / 2)];
    let header_height = (height / 12).max(1);
    let sidebar_width = (width / 5).max(1);

    for y in 0..height {
        let row = &mut y_plane[y * y_bytes_per_row..][..width];
        for (x, pixel) in row.iter_mut().enumerate() {
            let checker = ((x / 64 + y / 48) & 1) as u8;
            let mut value = 42 + checker * 7;
            if y < header_height {
                value = 92;
            } else if x < sidebar_width {
                value = 57 + ((y / 32) & 1) as u8 * 5;
            } else if y % 46 >= 17 && y % 46 < 21 && x % 96 < 68 {
                value = 154;
            }
            *pixel = value;
        }
    }

    for y in 0..height / 2 {
        let row = &mut uv_plane[y * uv_bytes_per_row..][..width];
        for x in (0..width).step_by(2) {
            let sidebar = x < sidebar_width;
            row[x] = if sidebar { 116 } else { 128 };
            row[x + 1] = if sidebar { 142 } else { 128 };
        }
    }
    (y_plane, uv_plane)
}

fn render_partial_motion(
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    layout: PlaneLayout,
    phase_frame: u64,
) {
    let rect_width = (layout.width / 3).max(2).min(layout.width);
    let rect_height = (layout.height / 3).max(2).min(layout.height);
    let horizontal_span = layout.width.saturating_sub(rect_width);
    let x_start = ping_pong(
        usize::try_from(phase_frame)
            .unwrap_or(usize::MAX)
            .saturating_mul(11),
        horizontal_span,
    );
    let y_start = layout.height.saturating_sub(rect_height) / 2;

    for y in y_start..y_start + rect_height {
        let row = &mut y_plane[y * layout.y_bytes_per_row..][..layout.width];
        for (x, pixel) in row[x_start..x_start + rect_width].iter_mut().enumerate() {
            let tile = ((x / 12 + y / 10 + phase_frame as usize / 2) & 1) as u8;
            *pixel = 48 + tile * 164;
        }
    }

    let uv_x_start = x_start / 2 * 2;
    let uv_x_end = (x_start + rect_width).div_ceil(2) * 2;
    let uv_y_start = y_start / 2;
    let uv_y_end = (y_start + rect_height).div_ceil(2);
    let chroma_step = u8::try_from(phase_frame % 48).unwrap_or(0);
    for y in uv_y_start..uv_y_end.min(layout.height / 2) {
        let row = &mut uv_plane[y * layout.uv_bytes_per_row..][..layout.width];
        for x in (uv_x_start..uv_x_end.min(layout.width)).step_by(2) {
            row[x] = 80_u8.saturating_add(chroma_step);
            row[x + 1] = 176_u8.saturating_sub(chroma_step);
        }
    }
}

fn render_full_motion(
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    layout: PlaneLayout,
    frame_index: u64,
) {
    const TILE_SIZE: usize = 16;
    for y in 0..layout.height {
        let row = &mut y_plane[y * layout.y_bytes_per_row..][..layout.width];
        let tile_y = y / TILE_SIZE;
        for x_start in (0..layout.width).step_by(TILE_SIZE) {
            let tile_x = x_start / TILE_SIZE;
            let state = tile_hash(tile_x, tile_y, frame_index);
            let value = 24 + u8::try_from(state % 8).unwrap_or(0) * 28;
            let x_end = (x_start + TILE_SIZE).min(layout.width);
            row[x_start..x_end].fill(value);
        }
    }
    for y in 0..layout.height / 2 {
        let row = &mut uv_plane[y * layout.uv_bytes_per_row..][..layout.width];
        let tile_y = y / (TILE_SIZE / 2);
        for x_start in (0..layout.width).step_by(TILE_SIZE) {
            let tile_x = x_start / TILE_SIZE;
            let state = tile_hash(tile_x, tile_y, frame_index ^ 0xa5a5_a5a5);
            let cb = 56 + u8::try_from(state % 9).unwrap_or(0) * 18;
            let cr = 56 + u8::try_from((state >> 8) % 9).unwrap_or(0) * 18;
            let x_end = (x_start + TILE_SIZE).min(layout.width);
            for x in (x_start..x_end).step_by(2) {
                row[x] = cb;
                row[x + 1] = cr;
            }
        }
    }
}

fn render_scene_cut(
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
    layout: PlaneLayout,
    phase_frame: u64,
    fps: u32,
) {
    let frames_per_scene = u64::from((fps / 2).max(1));
    let scene = phase_frame / frames_per_scene;
    for y in 0..layout.height {
        let row = &mut y_plane[y * layout.y_bytes_per_row..][..layout.width];
        for (x, pixel) in row.iter_mut().enumerate() {
            let value = match scene % 4 {
                0 => 32 + ((x * 7 / layout.width.max(1)) % 7) as u8 * 31,
                1 => 32 + ((y * 7 / layout.height.max(1)) % 7) as u8 * 31,
                2 => {
                    if ((x / 24) + (y / 24)) & 1 == 0 {
                        24
                    } else {
                        224
                    }
                }
                _ => 24 + ((x + y) % 208) as u8,
            };
            *pixel = value;
        }
    }

    let palettes = [(90, 240), (240, 110), (54, 34), (180, 70)];
    let (cb, cr) = palettes[usize::try_from(scene % 4).unwrap_or(0)];
    for y in 0..layout.height / 2 {
        let row = &mut uv_plane[y * layout.uv_bytes_per_row..][..layout.width];
        for x in (0..layout.width).step_by(2) {
            row[x] = cb;
            row[x + 1] = cr;
        }
    }
}

const fn align_up(value: usize, alignment: usize) -> Option<usize> {
    match value.checked_add(alignment - 1) {
        Some(value) => Some(value / alignment * alignment),
        None => None,
    }
}

fn ping_pong(position: usize, span: usize) -> usize {
    if span == 0 {
        return 0;
    }
    let cycle = span.saturating_mul(2);
    let position = position % cycle;
    if position <= span {
        position
    } else {
        cycle - position
    }
}

fn tile_hash(tile_x: usize, tile_y: usize, frame_index: u64) -> u64 {
    let mut value = frame_index
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add((tile_x as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9))
        .wrapping_add((tile_y as u64).wrapping_mul(0x94d0_49bb_1331_11eb));
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::{SyntheticFrameGenerator, SyntheticPhase, phase_for_frame};
    use screencapturekit::cm::IOSurfaceLockOptions;

    #[test]
    fn synthetic_cycle_visits_each_phase_and_repeats() {
        let fps = 30;
        assert_eq!(phase_for_frame(0, fps).0, SyntheticPhase::Static);
        assert_eq!(phase_for_frame(75, fps).0, SyntheticPhase::PartialMotion);
        assert_eq!(phase_for_frame(150, fps).0, SyntheticPhase::FullMotion);
        assert_eq!(phase_for_frame(225, fps).0, SyntheticPhase::SceneCuts);
        assert_eq!(phase_for_frame(300, fps).0, SyntheticPhase::Static);
    }

    #[test]
    fn partial_and_full_phases_change_progressively_more_pixels() {
        let mut generator = SyntheticFrameGenerator::new(64, 32, 4).unwrap();
        generator.render(0).unwrap();
        let static_frame = snapshot(&generator);
        generator.render(10).unwrap();
        let partial_frame = snapshot(&generator);
        generator.render(20).unwrap();
        let full_frame = snapshot(&generator);

        let partial_changes = changed_bytes(&static_frame, &partial_frame);
        let full_changes = changed_bytes(&static_frame, &full_frame);
        assert!(partial_changes > 0);
        assert!(partial_changes < full_changes);
        assert!(full_changes > static_frame.len() / 2);
    }

    #[test]
    fn full_motion_is_deterministic() {
        let mut first = SyntheticFrameGenerator::new(64, 32, 30).unwrap();
        let mut second = SyntheticFrameGenerator::new(64, 32, 30).unwrap();
        first.render(151).unwrap();
        second.render(151).unwrap();
        assert_eq!(snapshot(&first), snapshot(&second));
    }

    fn snapshot(generator: &SyntheticFrameGenerator) -> Vec<u8> {
        generator
            .surface()
            .lock(IOSurfaceLockOptions::READ_ONLY)
            .unwrap()
            .as_slice()
            .to_vec()
    }

    fn changed_bytes(left: &[u8], right: &[u8]) -> usize {
        left.iter()
            .zip(right)
            .filter(|(left, right)| left != right)
            .count()
    }
}
