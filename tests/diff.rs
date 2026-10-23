//! What `Pane::show` sends: for any change from one blocks content to the next, `edits::diff` gives edits that, applied
//! to the old, make the new (or says it can't, and all of it goes); and what it sends is small.

use buffa_types::google::protobuf::ListValue;
use pointman_extension::edits::{self, apply};
use pointman_extension::v1::block::Family;
use pointman_extension::v1::pane_edit::Edit;
use pointman_extension::v1::{chart, layout, list, Block, Chart, Layout, List, PaneContent, Text};

fn named(name: &str, family: Family) -> Block {
    Block { name: Some(name.into()), family: Some(family), ..Default::default() }
}

fn text(name: &str, markdown: &str) -> Block {
    named(name, Family::Text(Box::new(Text { markdown: markdown.into(), ..Default::default() })))
}

fn stack(name: &str, children: Vec<Block>) -> Block {
    let stack = layout::Stack { children, ..Default::default() };
    named(name, Family::Layout(Box::new(Layout { kind: Some(layout::Kind::Stack(Box::new(stack))), ..Default::default() })))
}

fn tabs(name: &str, tabs: Vec<(&str, &str, Vec<Block>)>) -> Block {
    let tabs = tabs
        .into_iter()
        .map(|(id, title, children)| layout::tabs::Tab { id: id.into(), title: title.into(), children, ..Default::default() })
        .collect();
    let tabs = layout::Tabs { tabs, ..Default::default() };
    named(name, Family::Layout(Box::new(Layout { kind: Some(layout::Kind::Tabs(Box::new(tabs))), ..Default::default() })))
}

fn items(name: &str, ids: &[(&str, &str)]) -> Block {
    let items = ids.iter().map(|(id, title)| list::Item { id: (*id).into(), title: (*title).into(), ..Default::default() });
    named(name, Family::List(Box::new(List { items: items.collect(), ..Default::default() })))
}

fn table(name: &str, columns: &[&str], rows: &[(&str, f64)]) -> Block {
    let data = chart::Data {
        columns: columns.iter().map(|c| c.to_string()).collect(),
        rows: rows.iter().map(|(_, v)| ListValue::from_values([*v])).collect(),
        row_ids: rows.iter().map(|(id, _)| id.to_string()).collect(),
        ..Default::default()
    };
    let chart = Chart { r#type: chart::Type::TYPE_TABLE.into(), data: data.into(), ..Default::default() };
    named(name, Family::Chart(Box::new(chart)))
}

fn content(blocks: Vec<Block>) -> PaneContent {
    PaneContent { blocks, ..Default::default() }
}

/// The edits for a change, checked: applying them makes the new content.
fn edits_for(old: Vec<Block>, new: Vec<Block>) -> Vec<pointman_extension::v1::PaneEdit> {
    let list = edits::diff(&old, &new).expect("edits can say it");
    let got = apply(&content(old), &list).expect("they apply");
    assert_eq!(got, content(new));
    list
}

fn kinds(list: &[pointman_extension::v1::PaneEdit]) -> Vec<&'static str> {
    list.iter()
        .map(|e| match e.edit.as_ref().unwrap() {
            Edit::Insert(_) => "insert",
            Edit::Replace(r) if r.keep_items => "replace+items",
            Edit::Replace(_) => "replace",
            Edit::Remove(_) => "remove",
            Edit::Move(_) => "move",
            Edit::PutItems(_) => "put_items",
            Edit::RemoveItems(_) => "remove_items",
        })
        .collect()
}

#[test]
fn nothing_changed_sends_nothing() {
    let blocks = vec![text("a", "A"), stack("s", vec![items("l", &[("1", "one")])])];
    assert!(edits_for(blocks.clone(), blocks).is_empty());
}

#[test]
fn a_changed_block_is_replaced_by_its_name() {
    let list = edits_for(vec![text("a", "A"), text("b", "B")], vec![text("a", "A"), text("b", "B!")]);
    assert_eq!(kinds(&list), ["replace"]);
}

#[test]
fn a_list_that_grew_sends_its_new_items_alone() {
    let old = vec![items("log", &[("t1", "Tick 1"), ("t2", "Tick 2")])];
    let new = vec![items("log", &[("t1", "Tick 1"), ("t2", "Tick 2"), ("t3", "Tick 3")])];
    let list = edits_for(old, new);
    assert_eq!(kinds(&list), ["put_items"]);
}

#[test]
fn items_that_changed_or_went_are_put_or_removed_by_id() {
    let old = vec![items("l", &[("a", "A"), ("b", "B"), ("c", "C")])];
    let new = vec![items("l", &[("a", "A!"), ("c", "C"), ("d", "D")])];
    assert_eq!(kinds(&edits_for(old, new)), ["remove_items", "put_items"]);
}

#[test]
fn items_in_another_order_send_the_whole_block() {
    let old = vec![items("l", &[("a", "A"), ("b", "B")])];
    let new = vec![items("l", &[("b", "B"), ("a", "A")])];
    assert_eq!(kinds(&edits_for(old, new)), ["replace"]);
}

#[test]
fn a_blocks_own_fields_go_alone_when_its_items_stay() {
    let old = vec![table("t", &["n"], &[("r1", 1.0), ("r2", 2.0)])];
    let new = vec![table("t", &["count"], &[("r1", 1.0), ("r2", 2.0), ("r3", 3.0)])];
    let list = edits_for(old, new);
    assert_eq!(kinds(&list), ["replace+items", "put_items"]);
    let Some(Edit::Replace(r)) = &list[0].edit else { panic!() };
    let Some(Family::Chart(c)) = r.block.as_option().unwrap().family.as_ref() else { panic!() };
    assert!(c.data.as_option().unwrap().rows.is_empty(), "the replace holds no rows: {c:?}");
}

#[test]
fn blocks_come_and_go_and_move() {
    let old = vec![text("a", "A"), stack("s", vec![text("x", "X"), text("y", "Y")]), text("z", "Z")];
    let new = vec![text("z", "Z"), stack("s", vec![text("y", "Y"), text("n", "N")]), text("a", "A"), text("x", "X")];
    edits_for(old, new);
}

#[test]
fn which_tab_is_open_is_the_persons() {
    let mut old = tabs("t", vec![("one", "One", vec![text("a", "A")]), ("two", "Two", vec![text("b", "B")])]);
    let mut new = old.clone();
    for (b, selected) in [(&mut old, "one"), (&mut new, "two")] {
        let Some(Family::Layout(l)) = b.family.as_mut() else { panic!() };
        let Some(layout::Kind::Tabs(t)) = l.kind.as_mut() else { panic!() };
        t.selected = Some(selected.into());
    }
    assert!(edits::diff(&[old], &[new]).unwrap().is_empty());
}

#[test]
fn a_tab_titled_anew_sends_the_tabs_whole_and_a_block_in_a_tab_changes_by_name() {
    let tabs_of = |title: &str, a: &str| tabs("t", vec![("one", title, vec![text("a", a)]), ("two", "Two", vec![items("l", &[("1", "x")])])]);
    assert_eq!(kinds(&edits_for(vec![tabs_of("One", "A")], vec![tabs_of("Uno", "A")])), ["replace"]);
    assert_eq!(kinds(&edits_for(vec![tabs_of("One", "A")], vec![tabs_of("One", "A!")])), ["replace"]);
}

#[test]
fn blocks_with_no_name_or_the_same_name_twice_cant_be_said_in_edits() {
    let mut unnamed = text("a", "A");
    unnamed.name = None;
    assert!(edits::diff(&[text("a", "A")], &[unnamed]).is_none());
    assert!(edits::diff(&[text("a", "A")], &[text("a", "A"), text("a", "B")]).is_none());
}

// ---- any change at all

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn lists_in(blocks: &[Block]) -> usize {
    1 + blocks.iter().map(|b| edits::children(b).into_iter().map(|k| lists_in(k)).sum::<usize>()).sum::<usize>()
}

fn nth_list<'a>(blocks: &'a mut Vec<Block>, n: &mut usize) -> Option<&'a mut Vec<Block>> {
    if *n == 0 {
        return Some(blocks);
    }
    *n -= 1;
    for b in blocks.iter_mut() {
        if let Some(Family::Layout(l)) = b.family.as_mut() {
            let kids: Vec<&mut Vec<Block>> = match l.kind.as_mut() {
                Some(layout::Kind::Stack(s)) => vec![&mut s.children],
                Some(layout::Kind::Tabs(t)) => t.tabs.iter_mut().map(|t| &mut t.children).collect(),
                None => vec![],
            };
            for k in kids {
                if let Some(found) = nth_list(k, n) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn fresh(rng: &mut Rng, counter: &mut u32, depth: usize) -> Block {
    *counter += 1;
    let name = format!("b{counter}");
    match rng.below(if depth > 2 { 4 } else { 6 }) {
        0 => text(&name, &format!("t{}", rng.below(3))),
        1 => {
            let n = rng.below(4);
            let ids: Vec<(String, String)> = (0..n).map(|i| (format!("{name}.i{i}"), format!("v{}", rng.below(3)))).collect();
            let refs: Vec<(&str, &str)> = ids.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
            items(&name, &refs)
        }
        2 => {
            let n = rng.below(4);
            let rows: Vec<(String, f64)> = (0..n).map(|i| (format!("{name}.r{i}"), rng.below(3) as f64)).collect();
            let refs: Vec<(&str, f64)> = rows.iter().map(|(a, b)| (a.as_str(), *b)).collect();
            table(&name, &["c"], &refs)
        }
        3 => text(&name, "x"),
        4 => {
            let n = rng.below(3);
            let kids = (0..n).map(|_| fresh(rng, counter, depth + 1)).collect();
            stack(&name, kids)
        }
        _ => {
            let n = 1 + rng.below(2);
            let tabs_in: Vec<(String, String, Vec<Block>)> = (0..n)
                .map(|i| (format!("{name}.tab{i}"), format!("Tab {}", rng.below(2)), (0..rng.below(3)).map(|_| fresh(rng, counter, depth + 1)).collect()))
                .collect();
            let refs = tabs_in.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), c.clone())).collect();
            tabs(&name, refs)
        }
    }
}

fn mutate(rng: &mut Rng, counter: &mut u32, blocks: &mut Vec<Block>) {
    let k = rng.below(lists_in(blocks));
    let list = nth_list(blocks, &mut k.clone()).unwrap();
    match rng.below(7) {
        0 => {
            let at = rng.below(list.len() + 1);
            let b = fresh(rng, counter, 1);
            list.insert(at, b);
        }
        1 if !list.is_empty() => {
            let at = rng.below(list.len());
            list.remove(at);
        }
        2 if !list.is_empty() => {
            // moved to another list, or within it: not into itself
            let at = rng.below(list.len());
            let b = list.remove(at);
            let k2 = rng.below(lists_in(blocks));
            let target = nth_list(blocks, &mut k2.clone()).unwrap();
            let at = rng.below(target.len() + 1);
            target.insert(at, b);
        }
        3 if list.len() > 1 => {
            let at = rng.below(list.len() - 1);
            list.swap(at, at + 1);
        }
        _ if !list.is_empty() => {
            let at = rng.below(list.len());
            let name = list[at].name.clone().unwrap();
            match list[at].family.as_mut() {
                Some(Family::Text(t)) => t.markdown = format!("m{}", rng.below(5)),
                Some(Family::List(l)) => match rng.below(4) {
                    0 => l.items.push(list::Item { id: format!("{name}.n{counter}"), title: "new".into(), ..Default::default() }),
                    1 if !l.items.is_empty() => drop(l.items.remove(rng.below(l.items.len()))),
                    2 if !l.items.is_empty() => {
                        let i = rng.below(l.items.len());
                        l.items[i].title = format!("v{}", rng.below(9));
                    }
                    _ if l.items.len() > 1 => l.items.swap(0, 1),
                    _ => l.style = list::Style::STYLE_CARDS.into(),
                },
                Some(Family::Chart(c)) => {
                    let d = c.data.get_or_insert_default();
                    match rng.below(4) {
                        0 => {
                            d.rows.push(ListValue::from_values([1.0]));
                            d.row_ids.push(format!("{name}.n{counter}"));
                        }
                        1 if !d.rows.is_empty() => {
                            let i = rng.below(d.rows.len());
                            d.rows.remove(i);
                            d.row_ids.remove(i);
                        }
                        2 if !d.rows.is_empty() => {
                            let i = rng.below(d.rows.len());
                            d.rows[i] = ListValue::from_values([rng.below(9) as f64]);
                        }
                        _ => c.title = format!("title {}", rng.below(4)),
                    }
                }
                Some(Family::Layout(l)) => match l.kind.as_mut() {
                    Some(layout::Kind::Stack(s)) => s.gap = Some(rng.below(3) as f64),
                    Some(layout::Kind::Tabs(t)) => match rng.below(2) {
                        0 => t.selected = Some(t.tabs[0].id.clone()),
                        _ => {
                            let i = rng.below(t.tabs.len());
                            t.tabs[i].title = format!("Tab {}", rng.below(9));
                        }
                    },
                    None => {}
                },
                _ => {}
            }
        }
        _ => {}
    }
    *counter += 1;
}

/// The same content but for which tab is open, which edits leave to the person.
fn without_selected(mut blocks: Vec<Block>) -> Vec<Block> {
    fn go(blocks: &mut [Block]) {
        for b in blocks {
            if let Some(Family::Layout(l)) = b.family.as_mut() {
                match l.kind.as_mut() {
                    Some(layout::Kind::Stack(s)) => go(&mut s.children),
                    Some(layout::Kind::Tabs(t)) => {
                        t.selected = None;
                        for tab in &mut t.tabs {
                            go(&mut tab.children);
                        }
                    }
                    None => {}
                }
            }
        }
    }
    go(&mut blocks);
    blocks
}

#[test]
fn any_change_is_said_in_edits_that_make_it_or_is_sent_whole() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let (mut said, mut whole, mut sent) = (0, 0, 0);
    for round in 0..4000 {
        let mut counter = 0;
        let old: Vec<Block> = (0..1 + rng.below(4)).map(|_| fresh(&mut rng, &mut counter, 0)).collect();
        let mut new = old.clone();
        for _ in 0..1 + rng.below(4) {
            mutate(&mut rng, &mut counter, &mut new);
        }
        match edits::diff(&old, &new) {
            Some(list) => {
                said += 1;
                sent += list.len();
                let got = apply(&content(old.clone()), &list)
                    .unwrap_or_else(|e| panic!("round {round}: {e}\nold: {old:#?}\nnew: {new:#?}\nedits: {list:#?}"));
                assert_eq!(without_selected(got.blocks), without_selected(new.clone()), "round {round}\nold: {old:#?}\nedits: {list:#?}");
            }
            None => whole += 1,
        }
    }
    assert!(said > 3500, "edits said {said} of 4000 changes (whole: {whole}, {sent} edits)");
}
