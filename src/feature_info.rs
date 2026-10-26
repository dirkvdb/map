use std::collections::HashSet;

use geo::Coordinate;
use gpui_kit::component::ActiveTheme;
use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div,
    rems,
};
use serde_json::Value;

/// A passive vector-feature inspector owned by the map view.
///
/// The parent positions this view and calls `clear` when the pointer leaves the
/// map. This view never takes focus or occludes the map's mouse hitbox.
#[derive(Default)]
pub struct FeatureInfo {
    coordinates: Option<String>,
    layer_name: String,
    features: Vec<FeatureSection>,
}

#[derive(Debug, PartialEq)]
struct FeatureSection {
    identity: String,
    id: Option<String>,
    properties: Vec<(SharedString, SharedString)>,
}

impl FeatureSection {
    fn from_features(features: Vec<Value>) -> Vec<Self> {
        let mut seen = HashSet::new();
        features
            .into_iter()
            .filter_map(|feature| {
                let id = feature.get("id").filter(|id| !id.is_null());
                // Tile clipping changes geometry, and fill/outline queries can
                // repeat a feature. Without an ID, retain geometry in the key
                // rather than merging distinct objects with identical properties.
                let identity = match id {
                    Some(id) => serde_json::json!(["id", id, feature.get("properties")]).to_string(),
                    None => serde_json::json!(["feature", feature]).to_string(),
                };
                if !seen.insert(identity.clone()) {
                    return None;
                }
                let properties = feature
                    .get("properties")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .map(|(key, value)| (SharedString::from(key.clone()), SharedString::from(property_text(value))))
                    .collect();
                Some(Self {
                    identity,
                    id: id.map(property_text),
                    properties,
                })
            })
            .collect()
    }
}

fn property_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        // Value's Display implementation preserves JSON types, including null,
        // booleans and nested arrays/objects, without debug escaping strings.
        _ => value.to_string(),
    }
}

impl FeatureInfo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hides the inspector and discards the previous hit properties.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if self.coordinates.take().is_some() {
            self.layer_name.clear();
            self.features.clear();
            cx.notify();
        }
    }

    /// Shows GeoJSON Features returned by `queryRenderedFeatures` for one layer.
    /// An empty result shows coordinates only. Unchanged presentation is a no-op.
    pub fn show(&mut self, coordinate: Coordinate, layer_name: &str, features: Vec<Value>, cx: &mut Context<Self>) {
        let coordinates = format!("Lat: {:.6}  Lon: {:.6}", coordinate.latitude, coordinate.longitude);
        let features = FeatureSection::from_features(features);
        let layer_name = if features.is_empty() { "" } else { layer_name };
        if self.coordinates.as_ref() == Some(&coordinates) && self.layer_name == layer_name && self.features == features {
            return;
        }
        self.coordinates = Some(coordinates);
        self.layer_name = layer_name.to_owned();
        self.features = features;
        cx.notify();
    }
}

impl Render for FeatureInfo {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut info = div().id("feature-info").test_support();
        let Some(coordinates) = &self.coordinates else {
            return info;
        };

        // Do not use block_mouse_except_scroll: it makes the map's hitbox stop
        // hovering underneath this passive overlay. Normal hitboxes allow map
        // pointer movement through while the bounded body owns wheel scrolling.
        info = info
            .flex()
            .flex_col()
            .max_w_full()
            .max_h(rems(24.0))
            .overflow_hidden()
            .text_xs()
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius_tokens().sm)
            .child(div().flex_shrink_0().px_2().py_1().child(coordinates.clone()));

        if !self.features.is_empty() {
            info = info
                .w(rems(26.0))
                .child(
                    div()
                        .flex_shrink_0()
                        .px_2()
                        .py_1()
                        .font_weight(FontWeight::MEDIUM)
                        .whitespace_normal()
                        .child(self.layer_name.clone()),
                )
                .child(
                    div()
                        .id("feature-info-properties")
                        .test_support()
                        .min_h_0()
                        .max_h(rems(20.0))
                        .overflow_y_scroll()
                        .children(self.features.iter().enumerate().map(|(ix, feature)| {
                            let mut section = div()
                                .flex()
                                .flex_col()
                                .flex_shrink_0()
                                .gap_1()
                                .p_2()
                                .border_t_1()
                                .border_color(cx.theme().border);
                            if self.features.len() > 1 {
                                let title = match &feature.id {
                                    Some(id) => format!("Feature {} · ID: {id}", ix + 1),
                                    None => format!("Feature {}", ix + 1),
                                };
                                section = section.child(div().font_weight(FontWeight::MEDIUM).child(title));
                            }
                            if feature.properties.is_empty() {
                                section = section.child(div().text_color(cx.theme().muted_foreground).child("No properties"));
                            } else {
                                section = section.children(feature.properties.iter().map(|(key, value)| {
                                    div()
                                        .flex()
                                        .items_start()
                                        .gap_2()
                                        .child(
                                            div()
                                                .w(rems(8.0))
                                                .flex_shrink_0()
                                                .whitespace_normal()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(key.clone()),
                                        )
                                        .child(div().flex_1().min_w_0().whitespace_normal().child(value.clone()))
                                }));
                            }
                            section
                        })),
                );
        }
        info
    }
}

#[cfg(test)]
mod tests {
    use super::{FeatureInfo, FeatureSection, property_text};
    use geo::Coordinate;
    use gpui_kit::{AppContext, TestAppContext};
    use serde_json::{Value, json};
    use std::cell::Cell;
    use std::rc::Rc;

    fn feature(id: Value, properties: Value, longitude: f64) -> Value {
        json!({
            "type": "Feature",
            "id": id,
            "properties": properties,
            "geometry": {"type": "Point", "coordinates": [longitude, 51.0]}
        })
    }

    #[test]
    fn property_values_preserve_json_types_and_string_contents() {
        for (value, expected) in [
            (json!(null), "null"),
            (json!(true), "true"),
            (json!(false), "false"),
            (json!(0), "0"),
            (json!(1.25), "1.25"),
            (json!("A \"quoted\" name\nnext line"), "A \"quoted\" name\nnext line"),
            (json!([null, true, "road", 3]), "[null,true,\"road\",3]"),
            (json!({"nested": {"value": false}}), "{\"nested\":{\"value\":false}}"),
        ] {
            assert_eq!(property_text(&value), expected);
        }
    }

    #[test]
    fn deduplicates_identified_tile_and_style_copies_but_not_different_properties() {
        let first = feature(json!(7), json!({"name": "Road"}), 4.0);
        let mut outline = first.clone();
        outline["layer"] = json!({"id": "outline"});
        let tile_copy = feature(json!(7), json!({"name": "Road"}), 4.1);
        let different_properties = feature(json!(7), json!({"name": "River"}), 4.0);
        let different_id = feature(json!(8), json!({"name": "Road"}), 4.0);
        let sections = FeatureSection::from_features(vec![first, outline, tile_copy, different_properties, different_id]);
        assert_eq!(sections.len(), 3);
        assert_eq!(sections[0].id.as_deref(), Some("7"));
        assert_eq!(sections[1].properties[0].1.as_ref(), "River");
    }

    #[test]
    fn idless_features_only_deduplicate_when_the_whole_feature_matches() {
        let mut first = feature(Value::Null, json!({"name": "Road"}), 4.0);
        first.as_object_mut().unwrap().remove("id");
        let mut distinct = first.clone();
        distinct["geometry"]["coordinates"][0] = json!(5.0);
        assert_eq!(FeatureSection::from_features(vec![first.clone(), first, distinct]).len(), 2);
    }

    #[test]
    fn missing_null_and_empty_properties_have_no_synthetic_property_rows() {
        let missing = json!({"type": "Feature", "id": "missing", "geometry": null});
        let null = feature(json!("null"), Value::Null, 4.0);
        let empty = feature(json!("empty"), json!({}), 4.0);
        let sections = FeatureSection::from_features(vec![missing, null, empty]);
        assert_eq!(sections.len(), 3);
        assert!(sections.iter().all(|section| section.properties.is_empty()));
    }

    #[gpui_kit::test]
    fn show_updates_properties_and_clear_discards_the_hover_snapshot(cx: &mut TestAppContext) {
        let info = cx.new(|_| FeatureInfo::new());
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                assert!(info.coordinates.is_none());
                info.show(
                    Coordinate::latlon(51.0, 4.0),
                    "Roads",
                    vec![feature(json!(1), json!({"name": "Main street", "open": true}), 4.0)],
                    cx,
                );
                assert_eq!(info.coordinates.as_deref(), Some("Lat: 51.000000  Lon: 4.000000"));
                assert_eq!(info.layer_name, "Roads");
                assert_eq!(info.features[0].properties[0].0.as_ref(), "name");
                assert_eq!(info.features[0].properties[0].1.as_ref(), "Main street");
                info.clear(cx);
                assert!(info.coordinates.is_none());
                assert!(info.layer_name.is_empty());
                assert!(info.features.is_empty());
            });
        });
    }

    #[gpui_kit::test]
    fn no_hit_replaces_stale_properties_with_coordinates_only(cx: &mut TestAppContext) {
        let info = cx.new(|_| FeatureInfo::new());
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                info.show(
                    Coordinate::latlon(51.0, 4.0),
                    "Roads",
                    vec![feature(json!(1), json!({"name": "Road"}), 4.0)],
                    cx,
                );
                info.show(Coordinate::latlon(52.0, 5.0), "Roads", vec![], cx);
                assert_eq!(info.coordinates.as_deref(), Some("Lat: 52.000000  Lon: 5.000000"));
                assert!(info.layer_name.is_empty());
                assert!(info.features.is_empty());
            });
        });
    }

    #[gpui_kit::test]
    fn repeated_results_and_redundant_clear_do_not_notify(cx: &mut TestAppContext) {
        let info = cx.new(|_| FeatureInfo::new());
        let observer = cx.new(|_| ());
        let notifications = Rc::new(Cell::new(0));
        let count = notifications.clone();
        let _subscription = observer.update(cx, |_, cx| cx.observe(&info, move |_, _, _| count.set(count.get() + 1)));
        let coordinate = Coordinate::latlon(51.0, 4.0);
        info.update(cx, |info, cx| {
            info.show(coordinate, "Roads", vec![feature(json!(1), json!({"name": "Road"}), 4.0)], cx);
        });
        cx.run_until_parked();
        assert_eq!(notifications.get(), 1);
        info.update(cx, |info, cx| {
            info.show(
                coordinate,
                "Roads",
                vec![feature(json!(1), json!({"name": "Road"}), 4.1), feature(json!(1), json!({"name": "Road"}), 4.2)],
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(notifications.get(), 1);
        info.update(cx, |info, cx| info.clear(cx));
        cx.run_until_parked();
        assert_eq!(notifications.get(), 2);
        info.update(cx, |info, cx| info.clear(cx));
        cx.run_until_parked();
        assert_eq!(notifications.get(), 2);
    }
}
