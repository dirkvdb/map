use crate::{Error, Result};

/// Tightly packed top-left-origin, premultiplied linear RGBA8 plus `[0,1]` depth.
#[derive(Debug)]
pub struct Surface {
    /// Physical pixel width, not logical native viewport width.
    pub width: u32,
    /// Physical pixel height.
    pub height: u32,
    /// Exactly `width * height * 4` premultiplied RGBA8 bytes, without row padding.
    pub color: Vec<u8>,
    /// Exactly `width * height` finite wgpu depth values in `[0,1]` from the
    /// GL-to-wgpu converted native projection. Native writes `2 * depth - 1`.
    pub depth: Vec<f32>,
    /// Extruded geometry tests and writes depth; flat geometry never writes depth.
    pub extruded: bool,
}
/// Alternative name for the CPU surface passed to the native renderer.
pub type NativeSurface = Surface;

impl Surface {
    /// Check packing, dimensions, and depth before any native allocation/upload.
    pub fn validate(&self) -> Result<()> {
        let pixels = pixel_count(self.width, self.height)?;
        let bytes = pixels.checked_mul(4).ok_or(Error::InvalidSurface("dimensions overflow"))?;
        if self.color.len() != bytes {
            return Err(Error::InvalidSurface("RGBA length mismatch"));
        }
        if self.depth.len() != pixels {
            return Err(Error::InvalidSurface("depth length mismatch"));
        }
        if self.depth.iter().any(|z| !z.is_finite() || !(0.0..=1.0).contains(z)) {
            return Err(Error::InvalidSurface("depth must be finite in [0,1]"));
        }
        Ok(())
    }
}
fn pixel_count(width: u32, height: u32) -> Result<usize> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidSurface("dimensions must be nonzero"));
    }
    // Shaders use uint indexing; reject wraparound even on 64-bit hosts.
    let pixels = width.checked_mul(height).ok_or(Error::InvalidSurface("dimensions overflow"))?;
    usize::try_from(pixels).map_err(|_| Error::InvalidSurface("dimensions overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn surface() -> Surface {
        Surface {
            width: 1,
            height: 1,
            color: vec![0, 0, 0, 0],
            depth: vec![1.0],
            extruded: false,
        }
    }
    #[test]
    fn accepts_transparent_packed_surface() {
        assert!(surface().validate().is_ok());
    }
    #[test]
    fn rejects_zero_dimensions() {
        let mut value = surface();
        value.width = 0;
        assert_eq!(value.validate(), Err(Error::InvalidSurface("dimensions must be nonzero")));
    }
    #[test]
    fn rejects_padded_color_rows() {
        let mut value = surface();
        value.color.resize(256, 0);
        assert_eq!(value.validate(), Err(Error::InvalidSurface("RGBA length mismatch")));
    }
    #[test]
    fn rejects_missing_depth() {
        let mut value = surface();
        value.depth.clear();
        assert_eq!(value.validate(), Err(Error::InvalidSurface("depth length mismatch")));
    }
    #[test]
    fn rejects_invalid_depth() {
        for depth in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
            let mut value = surface();
            value.depth[0] = depth;
            assert_eq!(value.validate(), Err(Error::InvalidSurface("depth must be finite in [0,1]")));
        }
    }
    #[test]
    fn rejects_shader_index_overflow() {
        assert_eq!(pixel_count(u32::MAX, 2), Err(Error::InvalidSurface("dimensions overflow")));
    }
}
