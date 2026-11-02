//! Shared native-compatible globe uniforms and procedural surface draw planning.

use bytemuck::{Pod, Zeroable};
use maplibre_native::WgpuLayerParameters;

use crate::raster_tiles::VisibleTile;

/// WGSL-compatible projection data appended to each raster draw uniform.
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct ProjectionUniform {
    pub globe_matrix: [[f32; 4]; 4],
    pub clipping_plane: [f32; 4],
    /// Normalized Mercator origin XY and size XY, retaining horizontal world wrap.
    pub tile_mercator: [f32; 4],
    /// Globe transition, native depth offset, grid sections, and enabled flag.
    pub parameters: [f32; 4],
}

impl ProjectionUniform {
    /// Copies native column-major globe data without changing the Mercator matrix.
    pub fn new(parameters: &WgpuLayerParameters, tile: VisibleTile) -> Self {
        let active = parameters.globe && parameters.projection_transition > 0.0;
        let size = (-f64::from(tile.z)).exp2();
        let sections = if !active {
            1.0
        } else {
            // Whole-world tiles need finer edges to track the curved horizon.
            match tile.z {
                0 => 128.0,
                1 => 64.0,
                _ => 32.0,
            }
        };
        Self {
            globe_matrix: std::array::from_fn(|column| std::array::from_fn(|row| parameters.globe_projection[column * 4 + row] as f32)),
            clipping_plane: parameters.globe_clipping_plane.map(|value| value as f32),
            tile_mercator: [(tile.x as f64 * size) as f32, (f64::from(tile.y) * size) as f32, size as f32, size as f32],
            parameters: [
                parameters.projection_transition.clamp(0.0, 1.0) as f32,
                parameters.depth_offset as f32,
                sections,
                f32::from(active),
            ],
        }
    }

    /// Whether this draw uses the globe or its transition rather than pure Mercator.
    pub fn active(&self) -> bool {
        self.parameters[3] > 0.0 && self.parameters[0] > 0.0
    }

    /// Non-indexed triangle-list vertex count for the shared procedural grid.
    /// Zero-initialized legacy uniforms still produce a single quad.
    pub fn vertex_count(&self) -> u32 {
        let sections = if self.active() { self.parameters[2].max(1.0) as u32 } else { 1 };
        6 * sections * sections
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parameters() -> WgpuLayerParameters {
        WgpuLayerParameters {
            width: 512.0,
            height: 512.0,
            latitude: 0.0,
            longitude: 0.0,
            zoom: 0.0,
            projection: [0.0; 16],
            mercator_projection: [0.0; 16],
            globe: true,
            projection_transition: 1.0,
            globe_projection: std::array::from_fn(|index| index as f64),
            globe_clipping_plane: [0.25, 0.5, 0.75, -0.125],
            depth_offset: 0.001953125,
            color_format: 0,
            depth_format: 0,
            depth_test: false,
        }
    }

    #[test]
    fn uniform_matches_wgsl_layout() {
        assert_eq!(
            [
                size_of::<ProjectionUniform>(),
                std::mem::offset_of!(ProjectionUniform, globe_matrix),
                std::mem::offset_of!(ProjectionUniform, clipping_plane),
                std::mem::offset_of!(ProjectionUniform, tile_mercator),
                std::mem::offset_of!(ProjectionUniform, parameters),
            ],
            [112, 0, 64, 80, 96],
        );
    }

    #[test]
    fn native_matrix_keeps_column_major_order() {
        let uniform = ProjectionUniform::new(&parameters(), VisibleTile { z: 2, x: 1, y: 3 });
        assert_eq!(
            uniform.globe_matrix,
            [[0.0, 1.0, 2.0, 3.0], [4.0, 5.0, 6.0, 7.0], [8.0, 9.0, 10.0, 11.0], [12.0, 13.0, 14.0, 15.0]]
        );
    }

    #[test]
    fn native_clipping_and_depth_offset_are_preserved() {
        let uniform = ProjectionUniform::new(&parameters(), VisibleTile { z: 2, x: 1, y: 3 });
        assert_eq!(
            (uniform.clipping_plane, uniform.parameters),
            ([0.25, 0.5, 0.75, -0.125], [1.0, 0.001953125, 32.0, 1.0])
        );
    }

    #[test]
    fn tile_coordinates_preserve_world_wrap() {
        for (x, expected) in [(-1, -0.25), (1, 0.25), (5, 1.25)] {
            let uniform = ProjectionUniform::new(&parameters(), VisibleTile { z: 2, x, y: 3 });
            assert_eq!(uniform.tile_mercator, [expected, 0.75, 0.25, 0.25]);
        }
    }

    #[test]
    fn active_globe_uses_finer_low_zoom_tessellation() {
        for (z, sections) in [(0, 128), (1, 64), (2, 32), (22, 32)] {
            let uniform = ProjectionUniform::new(&parameters(), VisibleTile { z, x: 0, y: 0 });
            assert_eq!(uniform.vertex_count(), 6 * sections * sections);
        }
    }

    #[test]
    fn mercator_and_completed_transition_keep_single_quad() {
        for (globe, transition) in [(false, 0.0), (false, 1.0), (true, 0.0)] {
            let mut parameters = parameters();
            parameters.globe = globe;
            parameters.projection_transition = transition;
            let uniform = ProjectionUniform::new(&parameters, VisibleTile { z: 0, x: 0, y: 0 });
            assert_eq!((uniform.active(), uniform.vertex_count()), (false, 6));
        }
    }

    #[test]
    fn blending_globe_remains_active_and_tessellated() {
        let mut parameters = parameters();
        parameters.projection_transition = 0.5;
        let uniform = ProjectionUniform::new(&parameters, VisibleTile { z: 2, x: 0, y: 0 });
        assert_eq!((uniform.active(), uniform.parameters[0], uniform.vertex_count()), (true, 0.5, 6144));
    }

    #[test]
    fn zeroed_legacy_uniform_draws_single_mercator_quad() {
        let uniform = ProjectionUniform::zeroed();
        assert_eq!((uniform.active(), uniform.vertex_count()), (false, 6));
    }
}
