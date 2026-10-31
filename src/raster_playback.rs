//! Commands and renderer feedback for temporal multiband raster playback.

use std::sync::Arc;

use anyhow::{Result, ensure};
use geo::RasterScale;
use inf::colormap::{ColorMapDirection, ColorMapPreset};

use crate::raster_source::RasterSource;

/// Runtime display overrides shared by every playback band, without rebuilding sources.
/// Unspecified properties retain each band's existing display style.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct PlaybackStyle {
    range: Option<(f32, f32)>,
    palette: Option<ColorMapPreset>,
    color_map_direction: Option<ColorMapDirection>,
    clamp_colors: Option<bool>,
    unscaling: Option<bool>,
    extrusion: Option<(bool, f32)>,
    extrusion_averaging: Option<bool>,
}

impl PlaybackStyle {
    pub fn with_range(mut self, range: (f32, f32)) -> Result<Self> {
        ensure!(
            range.0.is_finite() && range.1.is_finite() && (range.1 - range.0).is_finite() && range.0 < range.1,
            "Minimum must be less than maximum; both must be finite numbers."
        );
        self.range = Some(range);
        Ok(self)
    }

    pub fn with_palette(mut self, palette: ColorMapPreset) -> Self {
        self.palette = Some(palette);
        self
    }

    pub fn with_color_map_direction(mut self, direction: ColorMapDirection) -> Self {
        self.color_map_direction = Some(direction);
        self
    }

    pub fn with_clamp_colors(mut self, enabled: bool) -> Self {
        self.clamp_colors = Some(enabled);
        self
    }

    pub fn with_unscaling(mut self, enabled: bool) -> Self {
        self.unscaling = Some(enabled);
        self
    }

    pub fn with_extrusion(mut self, enabled: bool, height: f32) -> Self {
        self.extrusion = Some((enabled, crate::raster_source::normalized_extrusion_height(height)));
        self
    }

    /// Overrides averaging for all playback bands without rebuilding their sources.
    pub fn with_extrusion_averaging(mut self, enabled: bool) -> Self {
        self.extrusion_averaging = Some(enabled);
        self
    }

    /// The shared override, or None to retain each band's averaging setting.
    pub fn extrusion_averaging(self) -> Option<bool> {
        self.extrusion_averaging
    }

    /// Resolves averaging independently of whether extrusion is currently enabled.
    pub fn extrusion_averaging_for(self, source: &RasterSource) -> bool {
        self.extrusion_averaging().unwrap_or_else(|| source.is_extrusion_averaged())
    }

    pub fn extrusion(self) -> Option<(bool, f32)> {
        self.extrusion
    }

    pub fn extrusion_for(self, source: &RasterSource) -> (bool, f32) {
        self.extrusion().unwrap_or((source.is_extruded(), source.extrusion_height()))
    }

    pub fn range(self) -> Option<(f32, f32)> {
        self.range
    }
    pub fn palette(self) -> Option<ColorMapPreset> {
        self.palette
    }
    pub fn color_map_direction(self) -> Option<ColorMapDirection> {
        self.color_map_direction
    }
    pub fn clamp_colors(self) -> Option<bool> {
        self.clamp_colors
    }
    pub fn unscaling(self) -> Option<bool> {
        self.unscaling
    }

    pub fn range_for(self, source: &RasterSource) -> (f32, f32) {
        self.range().unwrap_or_else(|| source.range())
    }
    pub fn palette_for(self, source: &RasterSource) -> ColorMapPreset {
        self.palette().unwrap_or_else(|| source.palette())
    }
    pub fn color_map_direction_for(self, source: &RasterSource) -> ColorMapDirection {
        self.color_map_direction().unwrap_or_else(|| source.color_map_direction())
    }
    pub fn clamp_colors_for(self, source: &RasterSource) -> bool {
        self.clamp_colors().unwrap_or_else(|| source.clamp_colors())
    }
    pub fn unscaling_for(self, source: &RasterSource) -> bool {
        self.unscaling().unwrap_or_else(|| source.unscaling_enabled()) && source.has_scale_metadata()
    }
    pub fn scale_for(self, source: &RasterSource) -> RasterScale {
        if self.unscaling_for(source) {
            source.metadata().scale.unwrap_or_default()
        } else {
            RasterScale::default()
        }
    }
}

/// Playback reads one contiguous, 1-based band range of a single numeric raster file.
/// Native COGs and GDAL sources (GeoTIFF, VRT, single-variable NetCDF, …) qualify.
pub fn validate_bands(bands: &[Arc<RasterSource>]) -> Result<()> {
    let Some(first) = bands.first() else { return Ok(()) };
    let metadata = first.metadata();
    let start = metadata.band_nr;
    ensure!(start.is_some_and(|band| band > 0), "Multiband playback requires numeric raster bands");
    for (index, source) in bands.iter().enumerate() {
        let band = source.metadata();
        ensure!(band.path == metadata.path, "Playback bands must belong to the same dataset path");
        ensure!(
            band.band_nr == start.and_then(|start| start.checked_add(index)),
            "Playback bands must be contiguous and in dataset band order"
        );
        ensure!(
            band.tile_size.unwrap_or(256) == metadata.tile_size.unwrap_or(256),
            "Playback bands must have the same tile size"
        );
    }
    Ok(())
}

/// Immutable UI command. The renderer owns time advancement and buffering.
#[derive(Clone)]
pub struct PlaybackRequest {
    bands: Arc<[Arc<RasterSource>]>,
    position: f64,
    revision: u64,
    playing: bool,
    bands_per_second: f64,
    interpolation: bool,
    style: PlaybackStyle,
    data_range: Option<(f32, f32)>,
}

impl PlaybackRequest {
    pub fn new(bands: Vec<Arc<RasterSource>>, position: f64, revision: u64, playing: bool, bands_per_second: f64, interpolation: bool) -> Self {
        let style = PlaybackStyle::default();
        let data_range = playback_data_range(&bands, style);
        Self {
            bands: bands.into(),
            position,
            revision,
            playing,
            bands_per_second,
            interpolation,
            style: PlaybackStyle { range: data_range, ..style },
            data_range,
        }
    }

    /// The paused initial command, with a shared display range covering all playable bands.
    pub fn for_dataset(bands: &[Arc<RasterSource>]) -> Option<Self> {
        if bands.len() < 2 || validate_bands(bands).is_err() {
            return None;
        }
        let mut request = Self::new(bands.to_vec(), 0.0, 0, false, 3.0, true);
        request.style = request.style.with_range(request.data_range()?).ok()?;
        Some(request)
    }

    pub fn style(&self) -> PlaybackStyle {
        self.style
    }

    pub fn with_style(mut self, style: PlaybackStyle) -> Self {
        if self.style.unscaling() != style.unscaling() {
            self.data_range = playback_data_range(&self.bands, style);
        }
        self.style = PlaybackStyle {
            range: style.range().or(self.data_range),
            ..style
        };
        self
    }

    /// Combined bounds of every playback band in the current display units.
    pub fn data_range(&self) -> Option<(f32, f32)> {
        self.data_range
    }

    /// Changes playback controls while retaining the immutable band allocation and style.
    pub fn with_controls(mut self, position: f64, revision: u64, playing: bool, bands_per_second: f64) -> Self {
        self.position = position;
        self.revision = revision;
        self.playing = playing;
        self.bands_per_second = bands_per_second;
        self
    }

    /// Only the exact same immutable band allocation can reuse renderer validation.
    pub fn shares_bands(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.bands, &other.bands)
    }

    /// Seeds a replacement renderer from accepted feedback without issuing a seek.
    pub fn resumed_from(&self, frame: Option<PlaybackFrame>) -> Self {
        let mut request = self.clone();
        if let Some(frame) = frame.filter(|frame| Some(frame.dataset_id()) == self.dataset_id() && frame.revision() == self.revision) {
            request.position = frame.position();
        }
        request
    }

    pub fn bands(&self) -> &[Arc<RasterSource>] {
        &self.bands
    }
    pub fn dataset_id(&self) -> Option<tiler::LayerId> {
        self.bands.first().map(|band| band.metadata().id)
    }
    pub fn position(&self) -> f64 {
        self.position
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn is_playing(&self) -> bool {
        self.playing
    }
    pub fn bands_per_second(&self) -> f64 {
        self.bands_per_second
    }
    pub fn interpolation(&self) -> bool {
        self.interpolation
    }
}

fn playback_data_range(bands: &[Arc<RasterSource>], style: PlaybackStyle) -> Option<(f32, f32)> {
    bands
        .iter()
        .map(|source| source.data_range_with_scale(style.scale_for(source)))
        .reduce(|(min, max), (band_min, band_max)| (min.min(band_min), max.max(band_max)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources(directory: &std::path::Path, scaled: bool) -> Result<Vec<Arc<RasterSource>>> {
        let path = directory.join("styles.tif");
        crate::raster_source::test_support::write_raster(&path, 3857, 2)?;
        if scaled {
            std::fs::write(
                directory.join("styles.tif.aux.xml"),
                "<PAMDataset><PAMRasterBand band=\"1\"><Scale>-2</Scale><Offset>10</Offset></PAMRasterBand></PAMDataset>",
            )?;
        }
        RasterSource::open(&path)
    }

    #[test]
    fn range_validation_matches_source_semantics() -> Result<()> {
        for range in [(0.0, 0.0), (1.0, 0.0), (f32::NAN, 1.0), (0.0, f32::INFINITY), (-f32::MAX, f32::MAX)] {
            assert!(PlaybackStyle::default().with_range(range).is_err(), "Accepted invalid range {range:?}");
        }
        assert_eq!(PlaybackStyle::default().with_range((-10.0, 20.0))?.range(), Some((-10.0, 20.0)));
        Ok(())
    }

    #[test]
    fn default_style_preserves_each_bands_existing_properties() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let bands = sources(directory.path(), true)?;
        let second = bands[1]
            .restyled(ColorMapPreset::Gray, (-10.0, 1000.0))?
            .with_color_map_direction(ColorMapDirection::Reversed)?
            .with_clamp_colors(false)?;
        let style = PlaybackStyle::default();
        assert!(style.range().is_none() && style.palette().is_none() && style.color_map_direction().is_none());
        assert!(style.clamp_colors().is_none() && style.unscaling().is_none());
        for source in [&bands[0], &second] {
            assert_eq!(style.range_for(source), source.range());
            assert!(style.palette_for(source) == source.palette());
            assert!(style.color_map_direction_for(source) == source.color_map_direction());
            assert_eq!(style.clamp_colors_for(source), source.clamp_colors());
            assert_eq!(style.unscaling_for(source), source.unscaling_enabled());
            assert_eq!(style.scale_for(source), source.scale());
        }
        Ok(())
    }

    #[test]
    fn overrides_resolve_without_changing_sources_or_inventing_scale_metadata() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let bands = sources(directory.path(), true)?;
        let style = PlaybackStyle::default()
            .with_range((0.0, 100.0))?
            .with_palette(ColorMapPreset::Gray)
            .with_color_map_direction(ColorMapDirection::Reversed)
            .with_clamp_colors(false)
            .with_unscaling(true);
        assert_eq!(style.range_for(&bands[0]), (0.0, 100.0));
        assert!(style.palette() == Some(ColorMapPreset::Gray));
        assert!(style.color_map_direction() == Some(ColorMapDirection::Reversed));
        assert_eq!(style.clamp_colors(), Some(false));
        assert_eq!(style.unscaling(), Some(true));
        assert_eq!(style.scale_for(&bands[0]), RasterScale { scale: -2.0, offset: 10.0 });
        assert_eq!(style.with_unscaling(false).scale_for(&bands[0]), RasterScale::default());
        assert!(!style.unscaling_for(&bands[1]));
        assert_eq!(style.scale_for(&bands[1]), RasterScale::default());
        Ok(())
    }

    #[test]

    fn dataset_bounds_include_every_band_in_the_selected_units() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let bands = sources(directory.path(), true)?;
        let request = PlaybackRequest::for_dataset(&bands).unwrap();
        let expected = (
            bands[0].data_range().0.min(bands[1].data_range().0),
            bands[0].data_range().1.max(bands[1].data_range().1),
        );
        assert_eq!(request.data_range(), Some(expected));
        assert!(bands.iter().all(|band| request.style().range_for(band) == expected));
        let raw = request.clone().with_style(PlaybackStyle::default().with_unscaling(false));
        let first = bands[0].data_range_with_scale(RasterScale::default());
        let last = bands[1].data_range_with_scale(RasterScale::default());
        let expected_raw = (first.0.min(last.0), first.1.max(last.1));
        assert_eq!(raw.data_range(), Some(expected_raw));
        assert!(bands.iter().all(|band| raw.style().range_for(band) == expected_raw));
        assert!(raw.shares_bands(&request));
        let edited = raw.clone().with_style(raw.style().with_range((10.0, 30.0))?);
        assert_eq!(edited.data_range(), raw.data_range());
        assert_eq!(edited.style().range(), Some((10.0, 30.0)));
        Ok(())
    }

    #[test]
    fn extrusion_averaging_defaults_to_each_band_and_supports_both_overrides() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let bands = sources(directory.path(), false)?;
        let averaged = bands[1].with_extrusion_averaging(true)?;
        let style = PlaybackStyle::default();
        assert_eq!(style.extrusion_averaging(), None);
        assert!(!style.extrusion_averaging_for(&bands[0]));
        assert!(style.extrusion_averaging_for(&averaged));
        for enabled in [true, false] {
            let overridden = style.with_extrusion_averaging(enabled);
            assert_eq!(overridden.extrusion_averaging(), Some(enabled));
            assert_eq!(overridden.extrusion_averaging_for(&bands[0]), enabled);
            assert_eq!(overridden.extrusion_averaging_for(&averaged), enabled);
        }
        assert!(!bands[0].is_extrusion_averaged());
        assert!(averaged.is_extrusion_averaged());
        Ok(())
    }

    #[test]
    fn extrusion_averaging_override_survives_all_other_style_builders() -> Result<()> {
        let style = PlaybackStyle::default().with_extrusion_averaging(true);
        for restyled in [
            style.with_palette(ColorMapPreset::Gray),
            style.with_range((0.0, 100.0))?,
            style.with_color_map_direction(ColorMapDirection::Reversed),
            style.with_clamp_colors(false),
            style.with_unscaling(false),
            style.with_unscaling(true),
            style.with_extrusion(false, 2.5),
            style.with_extrusion(true, 7.0),
        ] {
            assert_eq!(restyled.extrusion_averaging(), Some(true));
        }
        Ok(())
    }

    #[test]
    fn dataset_commands_share_all_band_bounds_without_modifying_sources() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut bands = sources(directory.path(), true)?;
        bands[1] = bands[1].restyled(ColorMapPreset::Gray, (-1000.0, 2000.0))?;
        let first = bands[0].data_range();
        let second = bands[1].data_range();
        let expected = (first.0.min(second.0), first.1.max(second.1));
        let request = PlaybackRequest::for_dataset(&bands).unwrap();
        assert_eq!(request.data_range(), Some(expected));
        assert_eq!(request.style().range(), Some(expected));
        for (original, band) in bands.iter().zip(request.bands()) {
            assert!(Arc::ptr_eq(original, band));
            assert_eq!(request.style().range_for(band), expected);
        }
        assert_eq!(bands[1].range(), (-1000.0, 2000.0));
        Ok(())
    }

    #[test]
    fn dataset_bounds_follow_each_bands_units_but_not_style_ranges_or_position() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let bands = sources(directory.path(), true)?;
        let first = bands[0].data_range_with_scale(RasterScale::default());
        let second = bands[1].data_range_with_scale(RasterScale::default());
        let expected = (first.0.min(second.0), first.1.max(second.1));
        let request = PlaybackRequest::for_dataset(&bands).unwrap();
        let style = request.style().with_unscaling(false).with_range((0.0, 1.0))?;
        let request = request.with_style(style).with_controls(1.5, 1, true, 3.0);
        assert_eq!(request.data_range(), Some(expected));

        Ok(())
    }

    #[test]
    fn control_style_and_resume_builders_share_the_original_band_allocation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let bands = sources(directory.path(), false)?;
        let base = PlaybackRequest::new(bands.clone(), 0.0, 1, false, 2.0, true);
        let style = PlaybackStyle::default().with_range((10.0, 30.0))?;
        let changed = base.clone().with_style(style).with_controls(1.5, 7, true, 4.0);
        assert!(base.shares_bands(&changed));
        assert!(changed.style() == style);
        assert_eq!(
            (changed.position(), changed.revision(), changed.is_playing(), changed.bands_per_second()),
            (1.5, 7, true, 4.0)
        );
        assert!(changed.interpolation());
        let resumed = changed.resumed_from(Some(PlaybackFrame::new(changed.dataset_id().unwrap(), 7, 0.5, false)));
        assert!(base.shares_bands(&resumed) && resumed.style() == style);
        assert!(!base.shares_bands(&PlaybackRequest::new(bands, 0.0, 1, false, 2.0, true)));
        Ok(())
    }
}

/// Accepted renderer position, never a request to seek or start new work.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlaybackFrame {
    dataset_id: tiler::LayerId,
    revision: u64,
    position: f64,
    buffering: bool,
}

impl PlaybackFrame {
    pub fn new(dataset_id: tiler::LayerId, revision: u64, position: f64, buffering: bool) -> Self {
        Self {
            dataset_id,
            revision,
            position,
            buffering,
        }
    }
    pub fn dataset_id(self) -> tiler::LayerId {
        self.dataset_id
    }
    pub fn revision(self) -> u64 {
        self.revision
    }
    pub fn position(self) -> f64 {
        self.position
    }
    pub fn is_buffering(self) -> bool {
        self.buffering
    }
}
