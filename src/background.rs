use gpui_kit::component::ActiveTheme;
use gpui_kit::component::select::SelectItem;
use gpui_kit::{App, SharedString};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BasemapStyle {
    Light,
    Dark,
    OpenStreetMap,
    Empty,
    Tiles(&'static TileSet),
}

impl BasemapStyle {
    /// The "Default" background choice: the vector style matching the appearance mode.
    pub fn default_for_theme(cx: &App) -> Self {
        if cx.theme().mode.is_dark() { Self::Dark } else { Self::Light }
    }
    pub fn style_url(self) -> Option<&'static str> {
        match self {
            Self::Light => Some("https://basemaps.cartocdn.com/gl/positron-gl-style/style.json"),
            Self::Dark => Some("https://basemaps.cartocdn.com/gl/dark-matter-gl-style/style.json"),
            Self::OpenStreetMap => Some("https://tiles.openfreemap.org/styles/bright"),
            _ => None,
        }
    }

    pub fn style_json(self) -> Option<String> {
        let mut style = json!({ "version": 8, "sources": {}, "layers": [] });
        match self {
            Self::Light | Self::Dark | Self::OpenStreetMap => return None,
            Self::Empty => {}
            Self::Tiles(tiles) => {
                style["sources"]["background-tiles"] = tiles.source_json();
                style["layers"] = json!([{
                    "id": "background-tiles", "type": "raster", "source": "background-tiles"
                }]);
            }
        }
        Some(style.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileSet {
    name: &'static str,
    url: &'static str,
    min_zoom: u8,
    max_zoom: u8,
    flanders: bool,
}

impl TileSet {
    const fn new(name: &'static str, url: &'static str, min_zoom: u8, max_zoom: u8, flanders: bool) -> Self {
        Self {
            name,
            url,
            min_zoom,
            max_zoom,
            flanders,
        }
    }

    fn source_json(self) -> Value {
        let mut source = json!({
            "type": "raster", "tiles": [self.url], "tileSize": 256,
            "minzoom": self.min_zoom, "maxzoom": self.max_zoom, "scheme": "xyz"
        });
        if self.flanders {
            source["bounds"] = json!([2.53, 50.685, 5.92, 51.52]);
        }
        source
    }
}

#[derive(Clone)]
pub struct BackgroundChoice {
    name: &'static str,
    style: Option<BasemapStyle>,
}

impl SelectItem for BackgroundChoice {
    type Value = Option<BasemapStyle>;

    fn title(&self) -> SharedString {
        self.name.into()
    }

    fn value(&self) -> &Self::Value {
        &self.style
    }
}

pub fn choices() -> Vec<BackgroundChoice> {
    let mut choices = vec![
        BackgroundChoice { name: "Default", style: None },
        BackgroundChoice {
            name: "None",
            style: Some(BasemapStyle::Empty),
        },
        BackgroundChoice {
            name: "OpenStreetMap",
            style: Some(BasemapStyle::OpenStreetMap),
        },
    ];
    choices.extend(TILE_SETS.iter().map(|tiles| BackgroundChoice {
        name: tiles.name,
        style: Some(BasemapStyle::Tiles(tiles)),
    }));
    choices
}

// Remaining raster sources and limits from tileset.h; OSM uses vector tiles.
// The obsolete OSM alias and non-source sentinels are not separate choices.
static TILE_SETS: &[TileSet] = &[
    TileSet::new("Google Maps", "https://mt1.google.com/vt/lyrs=m&x={x}&y={y}&z={z}", 0, 22, false),
    TileSet::new("Google terrain", "https://mt1.google.com/vt/lyrs=p&x={x}&y={y}&z={z}", 0, 22, false),
    TileSet::new("Google satellite", "https://mt1.google.com/vt/lyrs=s&x={x}&y={y}&z={z}", 0, 22, false),
    TileSet::new("Google hybrid", "https://mt1.google.com/vt/lyrs=y&x={x}&y={y}&z={z}", 0, 22, false),
    TileSet::new(
        "Esri satellite",
        "https://server.arcgisonline.com/arcgis/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}",
        0,
        24,
        false,
    ),
    TileSet::new(
        "Flanders winter 2013–2015 (high resolution)",
        "https://geo.api.vlaanderen.be/OGW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=ogwrgb13_15vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        21,
        true,
    ),
    TileSet::new(
        "Flanders winter 2000–2003",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb00_03vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2005–2007",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb05_07vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2008–2011",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb08_11vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2012",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb12vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2013",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb13vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2014",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb14vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2015",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb15vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2016",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb16vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2017",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb17vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2018",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb18vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2019",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb19vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2020",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb20vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2021",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb21vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "Flanders winter 2022",
        "https://geo.api.vlaanderen.be/OMW/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=omwrgb22vl&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        22,
        true,
    ),
    TileSet::new(
        "GRB basemap",
        "https://geo.api.vlaanderen.be/GRB/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=grb_bsk&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        20,
        true,
    ),
    TileSet::new(
        "GRB basemap gray",
        "https://geo.api.vlaanderen.be/GRB/wmts?SERVICE=WMTS&REQUEST=GetTile&version=1.0.0&format=image/png&layer=grb_bsk_grijs&TILEMATRIXSET=GoogleMapsVL&TILEMATRIX={z}&TILECOL={x}&TILEROW={y}",
        0,
        20,
        true,
    ),
    TileSet::new(
        "Gewestplan",
        "https://mercator.vlaanderen.be/raadpleegdienstenmercatorpubliek/lu/lu_gwp_rv_raster/wms?REQUEST=GetMap&SERVICE=WMS&format=image/png&version=1.3.0&width=256&height=256&crs=EPSG:3857&layers=lu:lu_gwp_rv_raster&bbox={bbox-epsg-3857}&transparent=TRUE",
        0,
        24,
        true,
    ),
    TileSet::new(
        "CartoWeb topo",
        "https://cartoweb.wmts.ngi.be/1.0.0/topo/default/3857/{z}/{y}/{x}.png",
        7,
        17,
        false,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_vector_style_and_none_has_no_sources() {
        assert_eq!(choices()[0].style, None);
        assert!(BasemapStyle::Light.style_json().is_none());
        assert!(BasemapStyle::Dark.style_json().is_none());
        let empty: Value = serde_json::from_str(&BasemapStyle::Empty.style_json().unwrap()).unwrap();
        assert_eq!(empty, json!({ "version": 8, "sources": {}, "layers": [] }));
    }

    #[test]
    fn openstreetmap_uses_the_hosted_vector_style_instead_of_raster_tiles() {
        let choices = choices();
        let osm = choices.iter().find(|choice| choice.name == "OpenStreetMap").unwrap();
        assert_eq!(osm.style, Some(BasemapStyle::OpenStreetMap));
        assert_eq!(BasemapStyle::OpenStreetMap.style_url(), Some("https://tiles.openfreemap.org/styles/bright"));
        assert!(BasemapStyle::OpenStreetMap.style_json().is_none());
        assert!(!TILE_SETS.iter().any(|tiles| tiles.url.contains("tile.openstreetmap.org")));
        assert_eq!(choices.len(), 27);
    }

    #[test]
    fn catalogue_has_all_distinct_sources_and_valid_native_styles() {
        assert_eq!(TILE_SETS.len(), 24);
        let mut urls = std::collections::HashSet::new();
        for tiles in TILE_SETS {
            assert!(urls.insert(tiles.url), "Duplicate source: {}", tiles.name);
            let style: Value = serde_json::from_str(&BasemapStyle::Tiles(tiles).style_json().unwrap()).unwrap();
            let source = &style["sources"]["background-tiles"];
            assert_eq!(source["tiles"][0], tiles.url);
            assert_eq!(source["minzoom"], tiles.min_zoom);
            assert_eq!(source["maxzoom"], tiles.max_zoom);
            assert_eq!(source["tileSize"], 256);
            maplibre_native::AnySource::from_json_value("background-tiles", source).unwrap();
            maplibre_native::AnyLayer::from_json_value(&style["layers"][0]).unwrap();
        }
    }

    #[test]
    fn regional_bounds_and_wms_template_are_preserved() {
        let flanders = TILE_SETS.iter().find(|tiles| tiles.name == "Gewestplan").unwrap();
        assert_eq!(flanders.source_json()["bounds"], json!([2.53, 50.685, 5.92, 51.52]));
        assert!(flanders.url.contains("{bbox-epsg-3857}"));
        assert!(TILE_SETS[0].source_json().get("bounds").is_none());
    }
}
