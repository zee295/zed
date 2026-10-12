#[test]
fn markdown_keeps_editor_rules_without_a_parser() {
    let native = grammars::load_config_for_feature("markdown", true);
    let browser = grammars::load_config_for_feature("markdown", false);
    assert!(native.grammar.is_some());
    assert!(browser.grammar.is_none());
    assert_eq!(browser.unordered_list, native.unordered_list);
    assert!(!browser.ordered_list.is_empty());
    assert_eq!(
        browser.ordered_list[0].pattern,
        native.ordered_list[0].pattern
    );
    assert_eq!(
        browser.ordered_list[0].format,
        native.ordered_list[0].format
    );
    assert_eq!(
        browser.task_list.as_ref().unwrap().prefixes,
        native.task_list.as_ref().unwrap().prefixes
    );
    assert_eq!(
        browser.task_list.as_ref().unwrap().continuation,
        native.task_list.as_ref().unwrap().continuation
    );
    assert_eq!(
        browser
            .rewrap_prefixes
            .iter()
            .map(|rule| rule.as_str())
            .collect::<Vec<_>>(),
        native
            .rewrap_prefixes
            .iter()
            .map(|rule| rule.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(browser.brackets.pairs.len(), native.brackets.pairs.len());
    assert_eq!(browser.tab_size, native.tab_size);
}
