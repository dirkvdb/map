use std::collections::HashSet;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Icon, Sizable};
use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, TestSupportExt, Window, div,
    rems,
};
use serde_json::Value;

/// A vector-feature inspector owned and positioned by the map view.
///
/// The map mounts it while a vector layer is active and clears its properties
/// when the pointer leaves. Its surface stays passive so entering the pane or
/// its collapse button does not dismiss the hover.
#[derive(Default)]
pub struct FeatureInfo {
    layer_name: String,
    features: Vec<FeatureSection>,
    collapsed: bool,
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

    /// Discards the previous hit properties without hiding or expanding the pane.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        if !self.features.is_empty() {
            self.layer_name.clear();
            self.features.clear();
            cx.notify();
        }
    }

    /// Shows GeoJSON Features returned by `queryRenderedFeatures` for one layer.
    /// An empty result clears the properties. Unchanged presentation is a no-op.
    pub fn show(&mut self, layer_name: &str, features: Vec<Value>, cx: &mut Context<Self>) {
        let features = FeatureSection::from_features(features);
        let layer_name = if features.is_empty() { "" } else { layer_name };
        if self.layer_name == layer_name && self.features == features {
            return;
        }
        self.layer_name = layer_name.to_owned();
        self.features = features;
        cx.notify();
    }
}

impl Render for FeatureInfo {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep this pane passive so entering it does not clear the map's hover
        // snapshot. Only the bounded property list scrolls.
        let mut info = div()
            .id("feature-info")
            .test_support()
            .flex()
            .flex_col()
            .w_full()
            .max_h_full()
            .min_h_0()
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius_tokens().lg)
            .shadow(cx.theme().shadow_tokens().sm)
            .child(
                div()
                    .id("feature-info-heading")
                    .test_support()
                    .aria_label("Properties")
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .child(Icon::new(IconName::TableProperties).small())
                    .child(
                        div()
                            .id("feature-info-title")
                            .test_support()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Properties"),
                    )
                    .child(
                        Button::new("toggle-feature-info")
                            .small()
                            .ghost()
                            .icon(if self.collapsed { IconName::ChevronDown } else { IconName::ChevronUp })
                            .accessibility_label(if self.collapsed { "Expand properties" } else { "Collapse properties" })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.collapsed = !this.collapsed;
                                cx.notify();
                            })),
                    ),
            );
        if self.collapsed {
            return info;
        }
        if self.features.is_empty() {
            return info.child(
                div()
                    .id("feature-info-empty")
                    .test_support()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Hover a feature to inspect its properties."),
            );
        }
        info = info.pb_4().child(
            div()
                .id("feature-info-properties")
                .test_support()
                .min_h_0()
                .overflow_y_scroll()
                .text_xs()
                .children(self.features.iter().enumerate().map(|(ix, feature)| {
                    let mut section = div()
                        .id(SharedString::from(feature.identity.clone()))
                        .flex()
                        .flex_col()
                        .flex_shrink_0()
                        .gap_1()
                        .px_4()
                        .py_2()
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
                                .id(key.clone())
                                .test_support()
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
        info
    }
}

#[cfg(test)]
mod tests {
    use super::{FeatureInfo, FeatureSection, property_text};
    use gpui_kit::component::Theme;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext, TestAppContext, px, size};
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
    fn centered_header_collapses_with_pointer_and_keyboard_and_retains_state_across_hits(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        for font_size in [14.0, 18.0, 22.0] {
            cx.update(|cx| Theme::update(cx, |theme| theme.font_size = px(font_size)));
            let info = cx.new(|_| FeatureInfo::new());
            info.update(cx, |info, cx| info.show("Roads", vec![feature(json!(1), json!({"name": "Road"}), 4.0)], cx));
            let handle = cx.open_window(size(px(640.0), px(400.0)), |window, cx| gpui_kit::base::Root::new(info.clone(), window, cx));
            cx.update_window(handle.into(), |_, window, cx| {
                window.render_frame(cx);
                let heading = window.find("feature-info-heading").bounds();
                assert_eq!(window.find("feature-info-title").bounds().center().y, heading.center().y);
                // The component button snaps its frame to device pixels.
                let toggle_center = window.find("toggle-feature-info").bounds().center().y;
                assert!((toggle_center - heading.center().y).abs() <= px(0.5) / window.scale_factor());
                assert_eq!(window.find("feature-info-properties").bounds().top(), heading.bottom());

                window.click("toggle-feature-info", cx);
                assert!(info.read(cx).collapsed);
                assert!(window.try_find("feature-info-properties").is_none());
                assert_eq!(window.find("toggle-feature-info").label(), Some("Expand properties"));
                let collapsed_heading = window.find("feature-info-heading").bounds();
                assert_eq!(window.find("feature-info-title").bounds().center().y, collapsed_heading.center().y);

                info.update(cx, |info, cx| info.show("Roads", vec![feature(json!(2), json!({"name": "New road"}), 5.0)], cx));
                window.render_frame(cx);
                assert!(info.read(cx).collapsed);
                assert!(window.try_find("feature-info-properties").is_none());
                info.update(cx, |info, cx| info.clear(cx));
                window.render_frame(cx);
                assert!(window.find("feature-info-heading").visible());
                assert_eq!(window.find("toggle-feature-info").label(), Some("Expand properties"));
                assert!(window.try_find("feature-info-empty").is_none());
                info.update(cx, |info, cx| info.show("Rivers", vec![feature(json!(3), json!({"name": "River"}), 6.0)], cx));
                window.render_frame(cx);
                assert!(info.read(cx).collapsed);
                assert!(window.try_find("feature-info-properties").is_none());

                // This standalone passive view has no initial focus target.
                window.focus_next(cx);
                window.render_frame(cx);
                window.press("tab", cx);
                assert_eq!(window.find("toggle-feature-info").focused(), Some(true));
                window.press("enter", cx);
                assert!(!info.read(cx).collapsed);
                assert!(window.find("feature-info-properties").visible());
                assert_eq!(window.find("toggle-feature-info").label(), Some("Collapse properties"));
                assert_eq!(info.read(cx).features[0].properties[0].1.as_ref(), "River");
                window.press("space", cx);
                assert!(info.read(cx).collapsed);
                assert!(window.try_find("feature-info-properties").is_none());
                assert_eq!(window.find("toggle-feature-info").focused(), Some(true));
            })
            .unwrap();
        }
    }

    #[gpui_kit::test]
    fn empty_properties_keep_the_header_and_collapse_control_available(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let info = cx.new(|_| FeatureInfo::new());
        let handle = cx.open_window(size(px(640.0), px(400.0)), |window, cx| gpui_kit::base::Root::new(info.clone(), window, cx));
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            let heading = window.find("feature-info-heading").bounds();
            assert!(window.find("feature-info-empty").visible());
            assert!(window.try_find("feature-info-properties").is_none());
            window.click("toggle-feature-info", cx);
            assert!(info.read(cx).collapsed);
            assert!(window.try_find("feature-info-empty").is_none());
            assert_eq!(window.find("feature-info-heading").bounds(), heading);
            window.click("toggle-feature-info", cx);
            assert!(window.find("feature-info-empty").visible());

            info.update(cx, |info, cx| info.show("Roads", vec![feature(json!(1), json!({"name": "Road"}), 4.0)], cx));
            window.render_frame(cx);
            assert!(window.find("feature-info-properties").visible());
            info.update(cx, |info, cx| info.show("Roads", vec![], cx));
            window.render_frame(cx);
            assert!(window.try_find("feature-info-properties").is_none());
            assert!(window.find("feature-info-empty").visible());
            assert_eq!(window.find("feature-info-heading").bounds(), heading);
            window.click("toggle-feature-info", cx);
            assert!(info.read(cx).collapsed);
            assert_eq!(window.find("toggle-feature-info").label(), Some("Expand properties"));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn show_updates_properties_and_clear_discards_the_hover_snapshot(cx: &mut TestAppContext) {
        let info = cx.new(|_| FeatureInfo::new());
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                assert!(info.features.is_empty());
                info.show("Roads", vec![feature(json!(1), json!({"name": "Main street", "open": true}), 4.0)], cx);

                assert_eq!(info.layer_name, "Roads");
                assert_eq!(info.features[0].properties[0].0.as_ref(), "name");
                assert_eq!(info.features[0].properties[0].1.as_ref(), "Main street");
                info.clear(cx);

                assert!(info.layer_name.is_empty());
                assert!(info.features.is_empty());
            });
        });
    }

    #[gpui_kit::test]
    fn no_hit_clears_stale_properties(cx: &mut TestAppContext) {
        let info = cx.new(|_| FeatureInfo::new());
        cx.update(|cx| {
            info.update(cx, |info, cx| {
                info.show("Roads", vec![feature(json!(1), json!({"name": "Road"}), 4.0)], cx);
                info.show("Roads", vec![], cx);
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

        info.update(cx, |info, cx| {
            info.show("Roads", vec![feature(json!(1), json!({"name": "Road"}), 4.0)], cx);
        });
        cx.run_until_parked();
        assert_eq!(notifications.get(), 1);
        info.update(cx, |info, cx| {
            info.show(
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
