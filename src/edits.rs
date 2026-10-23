//! Pane edits (TYPES-9; panes.proto's `PaneEdit`): how they apply to a pane's content, and which to send for a change
//! from one content to the next.
//!
//! [`apply`] is core's `typed/pane_edits.py`, edit for edit: they apply in order, all or none, so one that can't (a
//! name or an item that isn't there, a name twice, an item of the wrong kind) is an error saying why, and the content
//! is as it was. [`diff`] and [`board_diff`] are what [`Pane::show`](crate::Pane::show) sends: only what changed.
//!
//! - A block is found by its `name` anywhere in the pane, inside stacks and tabs too. Names are unique in a pane.
//! - `parent` is a stack's name or a tab's id; unset, the top. `after` is a sibling's name there; unset, first.
//! - A block's items: a list's (or timeline's) items, a table's rows, a waterfall's spans, a traces block's rows. A
//!   board's items when `block` is unset. Each is put by its id: replaced where it is, or added at the end.
//! - `replace` with `keep_items` replaces the block's own fields and keeps its items (a waterfall's totals and state).

use std::collections::{HashMap, HashSet};

use buffa_types::google::protobuf::{value::Kind as ValueKind, ListValue, Struct, Value as ProtoValue};
use serde_json::Value;

use crate::convert;
use crate::v1::pane_edit::{self, item::Kind as ItemKind};
use crate::v1::{block::Family, chart, layout, view, Block, Chart, PaneContent, PaneEdit};

type Edit = pane_edit::Edit;

/// `content` with `edits` applied, as a new value; an error saying why if any can't apply.
pub fn apply(content: &PaneContent, edits: &[PaneEdit]) -> Result<PaneContent, String> {
    let mut out = content.clone();
    for (n, e) in edits.iter().enumerate() {
        one(&mut out, e).map_err(|why| format!("edit {}: {why}", n + 1))?;
    }
    Ok(out)
}

fn one(c: &mut PaneContent, e: &PaneEdit) -> Result<(), String> {
    match e.edit.as_ref() {
        Some(Edit::Insert(insert)) => {
            let b = insert.block.as_option().ok_or("an insert with no block")?;
            let name = b.name.as_deref().filter(|n| !n.is_empty()).ok_or("a block to insert has no name")?;
            if find(&c.blocks, name).is_some() {
                return Err(format!("there's a block {name:?} already"));
            }
            let have: HashSet<&str> = walk_all(&c.blocks).into_iter().filter_map(|b| b.name.as_deref()).collect();
            if walk(b).into_iter().filter_map(|x| x.name.as_deref()).any(|n| have.contains(n)) {
                return Err(format!("a block inside {name:?} has a name the pane has"));
            }
            place(c, insert.parent.as_deref(), insert.after.as_deref(), b.clone())
        }
        Some(Edit::Replace(replace)) => {
            let b = replace.block.as_option().ok_or("a replace with no block")?;
            let name = b.name.clone().unwrap_or_default();
            let (siblings, i) = find_mut(&mut c.blocks, &name).ok_or_else(|| format!("no block {name:?}"))?;
            let mut new = b.clone();
            if replace.keep_items {
                keep_items(&siblings[i], &mut new)?;
            }
            siblings[i] = new;
            Ok(())
        }
        Some(Edit::Remove(name)) => {
            let (siblings, i) = find_mut(&mut c.blocks, name).ok_or_else(|| format!("no block {name:?}"))?;
            siblings.remove(i);
            Ok(())
        }
        Some(Edit::Move(m)) => {
            let (siblings, i) = find_mut(&mut c.blocks, &m.block).ok_or_else(|| format!("no block {:?}", m.block))?;
            let b = siblings.remove(i);
            if let Some(parent) = &m.parent {
                if walk(&b).into_iter().any(|x| x.name.as_deref() == Some(parent)) {
                    return Err(format!("{:?} can't go inside itself", m.block));
                }
            }
            place(c, m.parent.as_deref(), m.after.as_deref(), b)
        }
        Some(Edit::PutItems(put)) => {
            let mut items = items_of(c, put.block.as_deref())?;
            for item in &put.items {
                items.put(item)?;
            }
            Ok(())
        }
        Some(Edit::RemoveItems(remove)) => {
            let mut items = items_of(c, remove.block.as_deref())?;
            for id in &remove.ids {
                items.remove(id)?;
            }
            Ok(())
        }
        None => Err("an edit of no kind this node knows".into()),
    }
}

// ------------------------------------------------------------------ the tree

/// The lists of blocks inside a block: a stack's children, each tab's.
pub fn children(b: &Block) -> Vec<&Vec<Block>> {
    match b.family.as_ref() {
        Some(Family::Layout(l)) => match l.kind.as_ref() {
            Some(layout::Kind::Stack(s)) => vec![&s.children],
            Some(layout::Kind::Tabs(t)) => t.tabs.iter().map(|t| &t.children).collect(),
            None => vec![],
        },
        _ => vec![],
    }
}

fn children_mut(b: &mut Block) -> Vec<&mut Vec<Block>> {
    match b.family.as_mut() {
        Some(Family::Layout(l)) => match l.kind.as_mut() {
            Some(layout::Kind::Stack(s)) => vec![&mut s.children],
            Some(layout::Kind::Tabs(t)) => t.tabs.iter_mut().map(|t| &mut t.children).collect(),
            None => vec![],
        },
        _ => vec![],
    }
}

/// A block and everything inside it, parents first.
pub fn walk(b: &Block) -> Vec<&Block> {
    let mut out = vec![b];
    for kids in children(b) {
        for k in kids {
            out.extend(walk(k));
        }
    }
    out
}

pub fn walk_all(blocks: &[Block]) -> Vec<&Block> {
    blocks.iter().flat_map(walk).collect()
}

/// The block with that name, anywhere.
pub fn find<'a>(blocks: &'a [Block], name: &str) -> Option<&'a Block> {
    for b in blocks {
        if b.name.as_deref() == Some(name) {
            return Some(b);
        }
        for kids in children(b) {
            if let Some(found) = find(kids, name) {
                return Some(found);
            }
        }
    }
    None
}

fn find_mut<'a>(blocks: &'a mut Vec<Block>, name: &str) -> Option<(&'a mut Vec<Block>, usize)> {
    if let Some(i) = blocks.iter().position(|b| b.name.as_deref() == Some(name)) {
        return Some((blocks, i));
    }
    for b in blocks.iter_mut() {
        for kids in children_mut(b) {
            if let Some(found) = find_mut(kids, name) {
                return Some(found);
            }
        }
    }
    None
}

/// The list a `parent` names: a stack's children, or a tab's by its id; the top for none.
fn parent_list<'a>(blocks: &'a mut Vec<Block>, parent: Option<&str>) -> Result<&'a mut Vec<Block>, String> {
    let Some(parent) = parent else { return Ok(blocks) };
    parent_in(blocks, parent).ok_or_else(|| format!("no stack or tab {parent:?}"))
}

fn parent_in<'a>(blocks: &'a mut Vec<Block>, parent: &str) -> Option<&'a mut Vec<Block>> {
    for i in 0..blocks.len() {
        if let Some(k) = list_named(&blocks[i], parent) {
            return children_mut(&mut blocks[i]).into_iter().nth(k);
        }
    }
    for b in blocks.iter_mut() {
        for kids in children_mut(b) {
            if let Some(found) = parent_in(kids, parent) {
                return Some(found);
            }
        }
    }
    None
}

/// Which of a block's lists (in `children`'s order) `parent` names: a stack by its own name, a tab by its id.
fn list_named(b: &Block, parent: &str) -> Option<usize> {
    match b.family.as_ref() {
        Some(Family::Layout(l)) => match l.kind.as_ref() {
            Some(layout::Kind::Stack(_)) if b.name.as_deref() == Some(parent) => Some(0),
            Some(layout::Kind::Tabs(t)) => t.tabs.iter().position(|t| t.id == parent),
            _ => None,
        },
        _ => None,
    }
}

fn place(c: &mut PaneContent, parent: Option<&str>, after: Option<&str>, b: Block) -> Result<(), String> {
    let siblings = parent_list(&mut c.blocks, parent)?;
    let at = match after {
        None => 0,
        Some(after) => {
            siblings.iter().position(|x| x.name.as_deref() == Some(after)).ok_or_else(|| format!("no block {after:?} there to follow"))? + 1
        }
    };
    siblings.insert(at, b);
    Ok(())
}

// ------------------------------------------------------------------ items

/// A block's items (or the board's), as one kind of list.
enum Items<'a> {
    List(&'a mut Vec<crate::v1::list::Item>),
    Table(&'a mut chart::Data),
    Spans(&'a mut Vec<crate::v1::waterfall_view::Span>),
    Traces(&'a mut Vec<crate::v1::traces_view::Row>),
    Board(&'a mut Vec<ProtoValue>),
}

impl Items<'_> {
    fn of(&self) -> &'static str {
        match self {
            Items::List(_) => "list",
            Items::Table(_) => "row",
            Items::Spans(_) => "span",
            Items::Traces(_) => "trace",
            Items::Board(_) => "board",
        }
    }

    fn index(&self, id: &str) -> Option<usize> {
        match self {
            Items::List(v) => v.iter().position(|i| i.id == id),
            Items::Table(d) => (0..d.rows.len()).find(|i| d.row_ids.get(*i).map(String::as_str).unwrap_or("") == id),
            Items::Spans(v) => v.iter().position(|i| i.id == id),
            Items::Traces(v) => v.iter().position(|i| i.id == id),
            Items::Board(v) => v.iter().position(|i| board_id(i) == id),
        }
    }

    fn put(&mut self, item: &pane_edit::Item) -> Result<(), String> {
        let got = match item.kind.as_ref() {
            Some(ItemKind::List(_)) => "list",
            Some(ItemKind::Row(_)) => "row",
            Some(ItemKind::Span(_)) => "span",
            Some(ItemKind::Trace(_)) => "trace",
            Some(ItemKind::Board(_)) => "board",
            None => "none",
        };
        if got != self.of() {
            return Err(format!("a {got} item for a block of {} items", self.of()));
        }
        let id = match item.kind.as_ref() {
            Some(ItemKind::List(i)) => i.id.clone(),
            Some(ItemKind::Row(i)) => i.id.clone(),
            Some(ItemKind::Span(i)) => i.id.clone(),
            Some(ItemKind::Trace(i)) => i.id.clone(),
            Some(ItemKind::Board(s)) => s.fields.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            None => String::new(),
        };
        if id.is_empty() {
            return Err("an item with no id".into());
        }
        let at = self.index(&id);
        match (self, item.kind.as_ref()) {
            (Items::List(v), Some(ItemKind::List(i))) => set(v, at, (**i).clone()),
            (Items::Spans(v), Some(ItemKind::Span(i))) => set(v, at, (**i).clone()),
            (Items::Traces(v), Some(ItemKind::Trace(i))) => set(v, at, (**i).clone()),
            (Items::Board(v), Some(ItemKind::Board(s))) => set(v, at, ProtoValue::from((**s).clone())),
            (Items::Table(d), Some(ItemKind::Row(row))) => {
                let cells = row.cells.as_option().cloned().unwrap_or_default();
                match at {
                    Some(i) => d.rows[i] = cells,
                    None => {
                        if d.row_ids.len() != d.rows.len() {
                            return Err("a table whose rows have no ids".into());
                        }
                        d.rows.push(cells);
                        d.row_ids.push(row.id.clone());
                    }
                }
            }
            _ => unreachable!("the kind was checked"),
        }
        Ok(())
    }

    fn remove(&mut self, id: &str) -> Result<(), String> {
        let i = self.index(id).ok_or_else(|| format!("no item {id:?}"))?;
        match self {
            Items::List(v) => drop(v.remove(i)),
            Items::Table(d) => {
                d.rows.remove(i);
                if i < d.row_ids.len() {
                    d.row_ids.remove(i);
                }
            }
            Items::Spans(v) => drop(v.remove(i)),
            Items::Traces(v) => drop(v.remove(i)),
            Items::Board(v) => drop(v.remove(i)),
        }
        Ok(())
    }
}

fn set<T>(v: &mut Vec<T>, at: Option<usize>, item: T) {
    match at {
        Some(i) => v[i] = item,
        None => v.push(item),
    }
}

fn board_id(v: &ProtoValue) -> &str {
    v.as_struct().and_then(|s| s.fields.get("id")).and_then(|v| v.as_str()).unwrap_or_default()
}

fn items_of<'a>(c: &'a mut PaneContent, name: Option<&str>) -> Result<Items<'a>, String> {
    let Some(name) = name else {
        let board = c.board.as_option_mut().ok_or("no board")?;
        let items = board.fields.get_mut("items").ok_or("a board with no items")?;
        return match items.kind.as_mut() {
            Some(ValueKind::ListValue(list)) => Ok(Items::Board(&mut list.values)),
            _ => Err("a board with no items".into()),
        };
    };
    let (siblings, i) = find_mut(&mut c.blocks, name).ok_or_else(|| format!("no block {name:?}"))?;
    match siblings[i].family.as_mut() {
        Some(Family::List(l)) => Ok(Items::List(&mut l.items)),
        Some(Family::Chart(ch)) if is_table(ch) => Ok(Items::Table(ch.data.get_or_insert_default())),
        Some(Family::View(v)) => match v.kind.as_mut() {
            Some(view::Kind::Waterfall(w)) => Ok(Items::Spans(&mut w.spans)),
            Some(view::Kind::Traces(t)) => Ok(Items::Traces(&mut t.items)),
            _ => Err(format!("{name:?} has no items")),
        },
        _ => Err(format!("{name:?} has no items")),
    }
}

fn is_table(c: &Chart) -> bool {
    c.r#type.as_known() == Some(chart::Type::TYPE_TABLE)
}

/// `new` with `old`'s items, for a replace that keeps them.
fn keep_items(old: &Block, new: &mut Block) -> Result<(), String> {
    let name = new.name.clone().unwrap_or_default();
    match (old.family.as_ref(), new.family.as_mut()) {
        (Some(Family::List(a)), Some(Family::List(b))) => b.items = a.items.clone(),
        (Some(Family::Chart(a)), Some(Family::Chart(b))) => {
            let data = b.data.get_or_insert_default();
            let old = a.data.as_option().cloned().unwrap_or_default();
            data.rows = old.rows;
            data.row_ids = old.row_ids;
        }
        (Some(Family::View(a)), Some(Family::View(b))) => match (a.kind.as_ref(), b.kind.as_mut()) {
            (Some(view::Kind::Waterfall(a)), Some(view::Kind::Waterfall(b))) => b.spans = a.spans.clone(),
            (Some(view::Kind::Traces(a)), Some(view::Kind::Traces(b))) => b.items = a.items.clone(),
            _ => return Err(format!("{name:?} has no items to keep")),
        },
        (Some(a), Some(b)) if std::mem::discriminant(a) != std::mem::discriminant(b) => {
            return Err(format!("{name:?} would keep its items as another family"))
        }
        _ => return Err(format!("{name:?} has no items to keep")),
    }
    Ok(())
}

// ------------------------------------------------------------------ what to send

/// A block with its own fields alone: no children, no items, and no tab in front (which is the person's).
fn shell(b: &Block) -> Block {
    let mut out = b.clone();
    for kids in children_mut(&mut out) {
        kids.clear();
    }
    if let Some(Family::Layout(l)) = out.family.as_mut() {
        if let Some(layout::Kind::Tabs(t)) = l.kind.as_mut() {
            t.selected = None;
        }
    }
    out
}

fn is_layout(b: &Block) -> bool {
    matches!(b.family, Some(Family::Layout(_)))
}

fn without_items(b: &Block) -> Block {
    let mut out = b.clone();
    match out.family.as_mut() {
        Some(Family::List(l)) => l.items.clear(),
        Some(Family::Chart(c)) => {
            if let Some(d) = c.data.as_option_mut() {
                d.rows.clear();
                d.row_ids.clear();
            }
        }
        Some(Family::View(v)) => match v.kind.as_mut() {
            Some(view::Kind::Waterfall(w)) => w.spans.clear(),
            Some(view::Kind::Traces(t)) => t.items.clear(),
            _ => {}
        },
        _ => {}
    }
    out
}

/// A block's items as (id, the item to put) pairs, if it has items and every one has an id, used once.
fn item_list(b: &Block) -> Option<Vec<(String, pane_edit::Item)>> {
    let items: Vec<(String, ItemKind)> = match b.family.as_ref()? {
        Family::List(l) => l.items.iter().map(|i| (i.id.clone(), ItemKind::List(Box::new(i.clone())))).collect(),
        Family::Chart(c) if is_table(c) => {
            let d = c.data.as_option()?;
            if d.row_ids.len() != d.rows.len() {
                return None;
            }
            d.rows
                .iter()
                .zip(&d.row_ids)
                .map(|(cells, id)| {
                    let row = pane_edit::Row { id: id.clone(), cells: cells.clone().into(), ..Default::default() };
                    (id.clone(), ItemKind::Row(Box::new(row)))
                })
                .collect()
        }
        Family::View(v) => match v.kind.as_ref()? {
            view::Kind::Waterfall(w) => w.spans.iter().map(|i| (i.id.clone(), ItemKind::Span(Box::new(i.clone())))).collect(),
            view::Kind::Traces(t) => t.items.iter().map(|i| (i.id.clone(), ItemKind::Trace(Box::new(i.clone())))).collect(),
            _ => return None,
        },
        _ => return None,
    };
    let mut seen = HashSet::new();
    if items.iter().any(|(id, _)| id.is_empty() || !seen.insert(id.clone())) {
        return None;
    }
    Some(items.into_iter().map(|(id, kind)| (id, pane_edit::Item { kind: Some(kind), ..Default::default() })).collect())
}

pub fn put_items(block: Option<&str>, items: Vec<pane_edit::Item>) -> PaneEdit {
    let put = pane_edit::Items { block: block.map(String::from), items, ..Default::default() };
    PaneEdit { edit: Some(Edit::PutItems(Box::new(put))), ..Default::default() }
}

pub fn remove_items(block: Option<&str>, ids: Vec<String>) -> PaneEdit {
    let remove = pane_edit::ItemIds { block: block.map(String::from), ids, ..Default::default() };
    PaneEdit { edit: Some(Edit::RemoveItems(Box::new(remove))), ..Default::default() }
}

pub fn replace(block: Block, keep_items: bool) -> PaneEdit {
    PaneEdit { edit: Some(Edit::Replace(Box::new(pane_edit::Replace { block: block.into(), keep_items, ..Default::default() }))), ..Default::default() }
}

/// What changed in one block (not a layout) from `old` to `new`: its items by id, and its own fields alone when
/// they changed too, or all of it when that's not possible.
fn block_edits(old: &Block, new: &Block) -> Vec<PaneEdit> {
    let whole = || vec![replace(new.clone(), false)];
    let name = new.name.as_deref().unwrap_or_default();
    let (Some(was), Some(now)) = (item_list(old), item_list(new)) else { return whole() };
    // the old ones that stay, in their order, and then the new: anything else moves, which an item can't
    let stays: Vec<&String> = was.iter().map(|(id, _)| id).filter(|id| now.iter().any(|(n, _)| n == *id)).collect();
    let order: Vec<&String> = now.iter().map(|(id, _)| id).take(stays.len()).collect();
    if stays != order {
        return whole();
    }
    let mut edits = Vec::new();
    if without_items(old) != without_items(new) {
        edits.push(replace(without_items(new), true));
    }
    let gone: Vec<String> = was.iter().map(|(id, _)| id.clone()).filter(|id| !now.iter().any(|(n, _)| n == id)).collect();
    if !gone.is_empty() {
        edits.push(remove_items(Some(name), gone));
    }
    let put: Vec<pane_edit::Item> = now
        .into_iter()
        .filter(|(id, item)| was.iter().find(|(w, _)| w == id).is_none_or(|(_, before)| before != item))
        .map(|(_, item)| item)
        .collect();
    if !put.is_empty() {
        edits.push(put_items(Some(name), put));
    }
    edits
}

/// Where a block is: the top of the pane, a stack by its name, or a tab by its id.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Parent {
    Top,
    Stack(String),
    Tab(String),
}

impl Parent {
    fn name(&self) -> Option<String> {
        match self {
            Parent::Top => None,
            Parent::Stack(n) | Parent::Tab(n) => Some(n.clone()),
        }
    }
}

/// Every list of blocks in a tree, by its parent, with the names in it, in order; and where each block is.
fn layout_of(blocks: &[Block]) -> Option<(HashMap<Parent, Vec<String>>, HashMap<String, Parent>, Vec<String>)> {
    fn go(
        blocks: &[Block],
        parent: Parent,
        lists: &mut HashMap<Parent, Vec<String>>,
        at: &mut HashMap<String, Parent>,
        order: &mut Vec<String>,
        tabs: &mut HashSet<String>,
    ) -> Option<()> {
        let mut names = Vec::new();
        for b in blocks {
            let name = b.name.clone().filter(|n| !n.is_empty())?;
            if at.insert(name.clone(), parent.clone()).is_some() {
                return None;
            }
            order.push(name.clone());
            names.push(name.clone());
            match b.family.as_ref() {
                Some(Family::Layout(l)) => match l.kind.as_ref() {
                    Some(layout::Kind::Stack(s)) => go(&s.children, Parent::Stack(name), lists, at, order, tabs)?,
                    Some(layout::Kind::Tabs(t)) => {
                        for tab in &t.tabs {
                            if tab.id.is_empty() || !tabs.insert(tab.id.clone()) {
                                return None;
                            }
                            go(&tab.children, Parent::Tab(tab.id.clone()), lists, at, order, tabs)?;
                        }
                    }
                    None => {}
                },
                _ => {}
            }
        }
        lists.insert(parent, names);
        Some(())
    }
    let (mut lists, mut at, mut order, mut tabs) = (HashMap::new(), HashMap::new(), Vec::new(), HashSet::new());
    go(blocks, Parent::Top, &mut lists, &mut at, &mut order, &mut tabs)?;
    // a tab's id must not be a block's name, or a parent could mean either
    if tabs.iter().any(|t| at.contains_key(t)) {
        return None;
    }
    Some((lists, at, order))
}

fn by_name(blocks: &[Block]) -> HashMap<String, &Block> {
    walk_all(blocks).into_iter().filter_map(|b| Some((b.name.clone()?, b))).collect()
}

/// The edits that take `old` blocks to `new`: blocks gone are removed, new ones inserted and moved ones moved, a
/// block with items sends its items by id (and its own fields alone, with `keep_items`, when those changed), and any
/// other changed block is replaced. Which tab is open is the person's, so `selected` never goes. None when there's no
/// way to say it in edits (a block with no name, a name twice): send all of it.
pub fn diff(old: &[Block], new: &[Block]) -> Option<Vec<PaneEdit>> {
    let (mut cur, mut loc, _) = layout_of(old)?;
    let (want, _, new_order) = layout_of(new)?;
    let (was, now) = (by_name(old), by_name(new));

    // a layout whose own fields changed is replaced whole, with what's in it: so nothing may come in or go out of it
    // by another way
    let mut whole: HashSet<&str> = HashSet::new();
    for name in &new_order {
        let (Some(a), Some(b)) = (was.get(name.as_str()), now.get(name.as_str())) else { continue };
        if (is_layout(a) || is_layout(b)) && (!(is_layout(a) && is_layout(b)) || shell(a) != shell(b)) {
            whole.insert(name);
        }
    }
    let inside = |root: &Block| -> HashSet<String> { walk(root).into_iter().filter_map(|b| b.name.clone()).collect() };
    for name in &whole {
        let (a, b) = (was[*name], now[*name]);
        let (before, after) = (inside(a), inside(b));
        if before.iter().any(|n| now.contains_key(n) && !after.contains(n)) || after.iter().any(|n| was.contains_key(n) && !before.contains(n)) {
            return None;
        }
    }
    let covered = |name: &str, tree: &HashMap<String, Parent>, roots: &HashMap<String, &Block>| -> bool {
        // inside a block that's replaced whole (not that block itself)
        let mut at = tree.get(name);
        while let Some(parent) = at {
            let Some(p) = parent.name().filter(|_| true) else { return false };
            let owner = match parent {
                Parent::Stack(n) => n.clone(),
                Parent::Tab(t) => roots
                    .iter()
                    .find(|(_, b)| matches!(b.family.as_ref(), Some(Family::Layout(l)) if matches!(l.kind.as_ref(), Some(layout::Kind::Tabs(tabs)) if tabs.tabs.iter().any(|x| x.id == *t))))
                    .map(|(n, _)| n.clone())
                    .unwrap_or_default(),
                Parent::Top => return false,
            };
            let _ = p;
            if whole.contains(owner.as_str()) {
                return true;
            }
            at = tree.get(&owner);
        }
        false
    };
    let new_at = layout_of(new)?.1;

    let mut edits = Vec::new();

    // placed: each list in the new tree, parents first, in order after the one before it
    let mut parents: Vec<Parent> = vec![Parent::Top];
    let mut inserted: HashSet<String> = HashSet::new();
    let mut i = 0;
    while i < parents.len() {
        let parent = parents[i].clone();
        i += 1;
        let names = want.get(&parent).cloned().unwrap_or_default();
        for (n, name) in names.iter().enumerate() {
            let prev = n.checked_sub(1).map(|p| names[p].clone());
            let b = now[name];
            let here = cur.get(&parent).cloned().unwrap_or_default();
            if loc.get(name) == Some(&parent) && !inserted.contains(name) {
                // already in this list: in place when, of those up to here, it follows the one before it
                let upto: HashSet<&String> = names[..=n].iter().collect();
                let seen: Vec<&String> = here.iter().filter(|x| upto.contains(x)).collect();
                let wanted: Vec<&String> = names[..=n].iter().collect();
                if seen != wanted {
                    let m = PaneEdit {
                        edit: Some(Edit::Move(Box::new(pane_edit::Move {
                            block: name.clone(),
                            parent: parent.name(),
                            after: prev.clone(),
                            ..Default::default()
                        }))),
                        ..Default::default()
                    };
                    edits.push(m);
                    relocate(&mut cur, &mut loc, name, &parent, prev.as_deref());
                }
            } else if was.contains_key(name.as_str()) && !inserted.contains(name) {
                edits.push(PaneEdit {
                    edit: Some(Edit::Move(Box::new(pane_edit::Move {
                        block: name.clone(),
                        parent: parent.name(),
                        after: prev.clone(),
                        ..Default::default()
                    }))),
                    ..Default::default()
                });
                relocate(&mut cur, &mut loc, name, &parent, prev.as_deref());
            } else {
                // new: all of it at once when nothing inside it is an old block that has to come in
                let all_new = walk(b).into_iter().filter_map(|x| x.name.as_deref()).all(|x| !was.contains_key(x));
                let to_send = if all_new { b.clone() } else { shell(b) };
                edits.push(PaneEdit {
                    edit: Some(Edit::Insert(Box::new(pane_edit::Insert {
                        block: to_send.into(),
                        parent: parent.name(),
                        after: prev.clone(),
                        ..Default::default()
                    }))),
                    ..Default::default()
                });
                let list = cur.entry(parent.clone()).or_default();
                let at = prev.as_ref().and_then(|p| list.iter().position(|x| x == p)).map_or(0, |p| p + 1);
                list.insert(at, name.clone());
                loc.insert(name.clone(), parent.clone());
                if all_new {
                    for x in walk(b) {
                        let x_name = x.name.clone().unwrap_or_default();
                        inserted.insert(x_name);
                    }
                } else {
                    inserted.insert(name.clone());
                    for tab in tabs_of(b) {
                        cur.entry(Parent::Tab(tab)).or_default();
                    }
                    cur.entry(Parent::Stack(name.clone())).or_default();
                }
            }
            // the lists inside it come next, unless it was sent whole
            if is_layout(b) && !whole.contains(name.as_str()) && !(inserted.contains(name) && walk(b).len() > 1 && walk(b).into_iter().skip(1).all(|x| inserted.contains(x.name.as_deref().unwrap_or_default()))) {
                match b.family.as_ref() {
                    Some(Family::Layout(l)) => match l.kind.as_ref() {
                        Some(layout::Kind::Stack(_)) => parents.push(Parent::Stack(name.clone())),
                        Some(layout::Kind::Tabs(t)) => parents.extend(t.tabs.iter().map(|t| Parent::Tab(t.id.clone()))),
                        None => {}
                    },
                    _ => {}
                }
            }
        }
    }

    // gone: the topmost old blocks that aren't in the new tree
    fn gone(blocks: &[Block], now: &HashMap<String, &Block>, whole: &HashSet<&str>, out: &mut Vec<PaneEdit>) {
        for b in blocks {
            let name = b.name.clone().unwrap_or_default();
            if !now.contains_key(&name) {
                out.push(PaneEdit { edit: Some(Edit::Remove(name)), ..Default::default() });
            } else if !whole.contains(name.as_str()) {
                for kids in children(b) {
                    gone(kids, now, whole, out);
                }
            }
        }
    }
    gone(old, &now, &whole, &mut edits);

    // changed: each block that was there and isn't a layout, or is one replaced whole
    for name in &new_order {
        let (Some(a), Some(b)) = (was.get(name.as_str()), now.get(name.as_str())) else { continue };
        if covered(name, &new_at, &now) {
            continue;
        }
        if whole.contains(name.as_str()) {
            edits.push(replace((*b).clone(), false));
        } else if !is_layout(b) && a != b {
            edits.extend(block_edits(a, b));
        }
    }
    Some(edits)
}

fn tabs_of(b: &Block) -> Vec<String> {
    match b.family.as_ref() {
        Some(Family::Layout(l)) => match l.kind.as_ref() {
            Some(layout::Kind::Tabs(t)) => t.tabs.iter().map(|t| t.id.clone()).collect(),
            _ => vec![],
        },
        _ => vec![],
    }
}

fn relocate(cur: &mut HashMap<Parent, Vec<String>>, loc: &mut HashMap<String, Parent>, name: &str, parent: &Parent, after: Option<&str>) {
    if let Some(from) = loc.get(name).cloned() {
        if let Some(list) = cur.get_mut(&from) {
            list.retain(|x| x != name);
        }
    }
    let list = cur.entry(parent.clone()).or_default();
    let at = after.and_then(|a| list.iter().position(|x| x == a)).map_or(0, |p| p + 1);
    list.insert(at, name.to_string());
    loc.insert(name.to_string(), parent.clone());
}

/// The edits that take a board's content to the next: its items by id (put when new or changed, removed when gone),
/// when only its items changed. None when the rest of it (its project, states, groups) changed: send all of it.
pub fn board_diff(old: &Value, new: &Value) -> Option<Vec<PaneEdit>> {
    let rest = |v: &Value| {
        let mut v = v.as_object().cloned().unwrap_or_default();
        v.remove("items");
        v
    };
    if rest(old) != rest(new) {
        return None;
    }
    let items = |v: &Value| -> Option<Vec<(String, Value)>> {
        let list = match v.get("items") {
            None | Some(Value::Null) => return Some(vec![]),
            Some(Value::Array(list)) => list,
            Some(_) => return None,
        };
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for item in list {
            let id = item.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
            item.as_object()?;
            if !seen.insert(id.to_string()) {
                return None;
            }
            out.push((id.to_string(), item.clone()));
        }
        Some(out)
    };
    let (was, now) = (items(old)?, items(new)?);
    let stays: Vec<&String> = was.iter().map(|(id, _)| id).filter(|id| now.iter().any(|(n, _)| n == *id)).collect();
    let order: Vec<&String> = now.iter().map(|(id, _)| id).take(stays.len()).collect();
    // the apps order cards by rank, so an item's place in the list isn't its place on the board; but an item put
    // lands at the end, so the order of the ones kept can't be said in edits
    if stays != order {
        return None;
    }
    let mut edits = Vec::new();
    let gone: Vec<String> = was.iter().map(|(id, _)| id.clone()).filter(|id| !now.iter().any(|(n, _)| n == id)).collect();
    if !gone.is_empty() {
        edits.push(remove_items(None, gone));
    }
    let put: Vec<pane_edit::Item> = now
        .iter()
        .filter(|(id, item)| was.iter().find(|(w, _)| w == id).is_none_or(|(_, before)| before != item))
        .map(|(_, item)| pane_edit::Item { kind: Some(ItemKind::Board(Box::new(convert::to_struct(item)))), ..Default::default() })
        .collect();
    if !put.is_empty() {
        edits.push(put_items(None, put));
    }
    Some(edits)
}

#[allow(dead_code)]
fn _unused(_: &ListValue, _: &Struct) {}

/// An edit that removes a block by its name.
pub fn remove(name: &str) -> PaneEdit {
    PaneEdit { edit: Some(Edit::Remove(name.to_string())), ..Default::default() }
}

/// An edit that puts these board items (JSON objects, each with its `id`).
pub fn put_board_items(items: &[Value]) -> PaneEdit {
    let items = items
        .iter()
        .map(|item| pane_edit::Item { kind: Some(ItemKind::Board(Box::new(convert::to_struct(item)))), ..Default::default() })
        .collect();
    put_items(None, items)
}
