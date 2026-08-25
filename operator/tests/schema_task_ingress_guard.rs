#[test]
fn schema_exposes_task_ingress_guard_with_required_image() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../schema/oabservice-v2.json"))
            .expect("valid schema JSON");

    let guard = &schema["$defs"]["ingress"]["properties"]["taskIngressGuard"];
    assert_eq!(guard["type"], "object");
    assert_eq!(guard["required"], serde_json::json!(["image"]));
    assert_eq!(guard["properties"]["image"]["type"], "string");
    assert_eq!(guard["additionalProperties"], false);
}
