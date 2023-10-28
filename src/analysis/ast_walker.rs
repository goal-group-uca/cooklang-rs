use std::borrow::Cow;
use std::collections::HashMap;

use regex::Regex;

use crate::ast::{self, IntermediateData, Modifiers, Text};
use crate::convert::{Converter, PhysicalQuantity};
use crate::error::{label, CowStr, PassResult, SourceDiag, SourceReport};
use crate::located::Located;
use crate::parser::{BlockKind, Event};
use crate::quantity::{Quantity, QuantityValue, ScalableValue, UnitInfo, Value};
use crate::span::Span;
use crate::{model::*, Extensions, RecipeRefCheckResult, RecipeRefChecker};

use super::{AnalysisResult, DefineMode, DuplicateMode};

macro_rules! error {
    ($msg:expr, $label:expr $(,)?) => {
        $crate::error::SourceDiag::error($msg, $label, $crate::error::Stage::Analysis)
    };
}

macro_rules! warning {
    ($msg:expr, $label:expr $(,)?) => {
        $crate::error::SourceDiag::warning($msg, $label, $crate::error::Stage::Analysis)
    };
}

#[tracing::instrument(level = "debug", skip_all, target = "cooklang::analysis")]
pub fn parse_events<'i>(
    events: impl Iterator<Item = Event<'i>>,
    extensions: Extensions,
    converter: &Converter,
    recipe_ref_checker: Option<RecipeRefChecker>,
) -> AnalysisResult {
    let mut ctx = SourceReport::empty();
    let temperature_regex = extensions
        .contains(Extensions::TEMPERATURE)
        .then(|| match converter.temperature_regex() {
            Ok(re) => Some(re),
            Err(err) => {
                ctx.warn(
                    SourceDiag::unlabeled(
                        "An error ocurred searching temperature values",
                        crate::error::Severity::Error,
                        crate::error::Stage::Analysis,
                    )
                    .set_source(err),
                );
                None
            }
        })
        .flatten();

    let col = RecipeCollector {
        extensions,
        temperature_regex,
        converter,
        recipe_ref_checker,

        content: ScalableRecipe {
            metadata: Default::default(),
            sections: Default::default(),
            ingredients: Default::default(),
            cookware: Default::default(),
            timers: Default::default(),
            inline_quantities: Default::default(),
            data: (),
        },
        current_section: Section::default(),

        define_mode: DefineMode::All,
        duplicate_mode: DuplicateMode::New,
        auto_scale_ingredients: false,
        ctx,

        locations: Default::default(),
        step_counter: 1,
    };
    col.parse_events(events)
}

struct RecipeCollector<'i, 'c> {
    extensions: Extensions,
    temperature_regex: Option<&'c Regex>,
    converter: &'c Converter,
    recipe_ref_checker: Option<RecipeRefChecker<'c>>,

    content: ScalableRecipe,
    current_section: Section,

    define_mode: DefineMode,
    duplicate_mode: DuplicateMode,
    auto_scale_ingredients: bool,
    ctx: SourceReport,

    locations: Locations<'i>,
    step_counter: u32,
}

#[derive(Default)]
struct Locations<'i> {
    ingredients: Vec<Located<ast::Ingredient<'i>>>,
    cookware: Vec<Located<ast::Cookware<'i>>>,
    metadata: HashMap<Cow<'i, str>, (Text<'i>, Text<'i>)>,
}

const IMPLICIT_REF_WARN: &str = "The reference (&) is implicit";

impl<'i, 'c> RecipeCollector<'i, 'c> {
    fn parse_events(mut self, mut events: impl Iterator<Item = Event<'i>>) -> AnalysisResult {
        let mut items = Vec::new();
        let events = events.by_ref();
        while let Some(event) = events.next() {
            match event {
                Event::Metadata { key, value } => self.metadata(key, value),
                Event::Section { name } => {
                    self.step_counter = 1;
                    if !self.current_section.is_empty() {
                        self.content.sections.push(self.current_section);
                    }
                    self.current_section =
                        Section::new(name.map(|t| t.text_trimmed().into_owned()));
                }
                Event::Start(_kind) => items.clear(),
                Event::End(kind) => {
                    let new_content = match kind {
                        _ if self.define_mode == DefineMode::Text => {
                            Content::Text(self.text_block(items))
                        }
                        BlockKind::Step => Content::Step(self.step(items)),
                        BlockKind::Text => Content::Text(self.text_block(items)),
                    };

                    // If define mode is ingredients, don't add the
                    // step to the section. The components should have been
                    // added to their lists
                    if self.define_mode != DefineMode::Components || new_content.is_text() {
                        if new_content.is_step() {
                            self.step_counter += 1;
                        }
                        self.current_section.content.push(new_content);
                    }

                    items = Vec::new();
                }
                item @ (Event::Text(_)
                | Event::Ingredient(_)
                | Event::Cookware(_)
                | Event::Timer(_)) => items.push(item),

                Event::Error(e) => {
                    // on a parser error, collect all other parser errors and
                    // warnings
                    self.ctx.error(e);
                    events.for_each(|e| match e {
                        Event::Error(e) | Event::Warning(e) => self.ctx.push(e),
                        _ => {}
                    });
                    // discard non parser errors/warnings
                    self.ctx.retain(|e| e.stage == crate::error::Stage::Parse);
                    // return no output
                    return PassResult::new(None, self.ctx);
                }
                Event::Warning(w) => self.ctx.warn(w),
            }
        }
        if !self.current_section.is_empty() {
            self.content.sections.push(self.current_section);
        }
        PassResult::new(Some(self.content), self.ctx)
    }

    fn metadata(&mut self, key: Text<'i>, value: Text<'i>) {
        self.locations
            .metadata
            .insert(key.text_trimmed(), (key.clone(), value.clone()));

        let key_t = key.text_trimmed();
        let value_t = value.text_outer_trimmed();
        let invalid_value = |possible| {
            error!(
                format!("Invalid value for config key '{key_t}': {value_t}"),
                label!(value.span(), "this value is not supported")
            )
            .label(label!(key.span(), "by this key"))
            .hint(format!("Possible values are: {possible:?}"))
        };

        if self.extensions.contains(Extensions::MODES)
            && key_t.starts_with('[')
            && key_t.ends_with(']')
        {
            let special_key = &key_t[1..key_t.len() - 1];
            match special_key {
                "define" | "mode" => match value_t.as_ref() {
                    "all" | "default" => self.define_mode = DefineMode::All,
                    "components" | "ingredients" => self.define_mode = DefineMode::Components,
                    "steps" => self.define_mode = DefineMode::Steps,
                    "text" => self.define_mode = DefineMode::Text,
                    _ => self
                        .ctx
                        .error(invalid_value(vec!["all", "components", "steps", "text"])),
                },
                "duplicate" => match value_t.as_ref() {
                    "new" | "default" => self.duplicate_mode = DuplicateMode::New,
                    "reference" | "ref" => self.duplicate_mode = DuplicateMode::Reference,
                    _ => self.ctx.error(invalid_value(vec!["new", "reference"])),
                },
                "auto scale" | "auto_scale" => match value_t.as_ref() {
                    "true" => self.auto_scale_ingredients = true,
                    "false" | "default" => self.auto_scale_ingredients = false,
                    _ => self.ctx.error(invalid_value(vec!["true", "false"])),
                },
                _ => {
                    self.ctx.warn(
                        warning!(
                            format!("Unknown config metadata key: {key_t}"),
                            label!(key.span())
                        )
                        .hint(
                            "Possible config keys are '[mode]', '[duplicate]' and '[auto scale]'",
                        ),
                    );
                    self.content
                        .metadata
                        .map
                        .insert(key_t.into_owned(), value_t.into_owned());
                }
            }
        } else if let Err(warn) = self
            .content
            .metadata
            .insert(key_t.into_owned(), value_t.into_owned())
        {
            self.ctx.warn(
                warning!(
                    format!(
                        "Unsupported value for special key: '{}'",
                        key.text_trimmed()
                    ),
                    label!(value.span(), "this value"),
                )
                .label(label!(key.span(), "is not supported by this key"))
                .hint("It will be a regular metadata entry")
                .set_source(warn),
            );
        }
    }

    fn step(&mut self, items: Vec<Event<'i>>) -> Step {
        let mut new_items = Vec::new();

        for item in items {
            match item {
                Event::Text(text) => {
                    let t = text.text();
                    if self.define_mode == DefineMode::Components {
                        // only issue warnings for alphanumeric characters
                        // so that the user can format the text with spaces,
                        // hypens or whatever.
                        if t.contains(|c: char| c.is_alphanumeric()) {
                            self.ctx.warn(warning!(
                                "Ignoring text in define components mode",
                                label!(text.span())
                            ));
                        }
                        continue; // ignore text
                    }

                    // it's only some if the extension is enabled
                    if let Some(re) = &self.temperature_regex {
                        debug_assert!(self.extensions.contains(Extensions::TEMPERATURE));

                        let mut haystack = t.as_ref();
                        while let Some((before, temperature, after)) =
                            find_temperature(haystack, re)
                        {
                            if !before.is_empty() {
                                new_items.push(Item::Text {
                                    value: before.to_string(),
                                });
                            }

                            new_items.push(Item::InlineQuantity {
                                index: self.content.inline_quantities.len(),
                            });
                            self.content.inline_quantities.push(temperature);

                            haystack = after;
                        }
                        if !haystack.is_empty() {
                            new_items.push(Item::Text {
                                value: haystack.to_string(),
                            });
                        }
                    } else {
                        new_items.push(Item::Text {
                            value: t.into_owned(),
                        });
                    }
                }

                Event::Ingredient(..) | Event::Cookware(..) | Event::Timer(..) => {
                    let new_component = match item {
                        Event::Ingredient(i) => Item::Ingredient {
                            index: self.ingredient(i),
                        },
                        Event::Cookware(c) => Item::Cookware {
                            index: self.cookware(c),
                        },
                        Event::Timer(t) => Item::Timer {
                            index: self.timer(t),
                        },
                        _ => unreachable!(),
                    };
                    new_items.push(new_component);
                }
                _ => panic!("Unexpected event in step: {item:?}"),
            };
        }

        Step {
            items: new_items,
            number: self.step_counter,
        }
    }

    fn text_block(&mut self, items: Vec<Event<'i>>) -> String {
        let mut s = String::new();
        for ev in items {
            match ev {
                Event::Text(t) => s += t.text().as_ref(),
                Event::Ingredient(_) | Event::Cookware(_) | Event::Timer(_) => {
                    assert_eq!(
                        self.define_mode,
                        DefineMode::Text,
                        "Non text event in text block outside define mode text"
                    );

                    // ignore component
                    let (c, s) = match ev {
                        Event::Ingredient(i) => ("ingredient", i.span()),
                        Event::Cookware(c) => ("cookware", c.span()),
                        Event::Timer(t) => ("timer", t.span()),
                        _ => unreachable!(),
                    };
                    self.ctx
                        .warn(warning!(format!("Ignoring {c} in text mode"), label!(s)));
                }
                _ => panic!("Unexpected event in text block: {ev:?}"),
            }
        }
        s
    }

    fn ingredient(&mut self, ingredient: Located<ast::Ingredient<'i>>) -> usize {
        let located_ingredient = ingredient.clone();
        let (ingredient, location) = ingredient.take_pair();

        let name = ingredient.name.text_trimmed();

        let mut new_igr = Ingredient {
            name: name.into_owned(),
            alias: ingredient.alias.map(|t| t.text_trimmed().into_owned()),
            quantity: ingredient.quantity.clone().map(|q| self.quantity(q, true)),
            note: ingredient.note.map(|n| n.text_trimmed().into_owned()),
            modifiers: ingredient.modifiers.into_inner(),
            relation: IngredientRelation::definition(
                Vec::new(),
                self.define_mode != DefineMode::Components,
            ),
        };

        if let Some(inter_data) = ingredient.intermediate_data {
            assert!(new_igr.modifiers().contains(Modifiers::REF));
            let invalid_modifiers = Modifiers::RECIPE | Modifiers::HIDDEN | Modifiers::NEW;
            if new_igr.modifiers().intersects(invalid_modifiers) {
                self.ctx.error(
                    error!(
                        "Conflicting modifiers with intermediate preparation reference",
                        label!(ingredient.modifiers.span())
                    )
                    .hint(format!(
                        "Remove the following modifiers: {}",
                        new_igr.modifiers() & invalid_modifiers
                    )),
                );
            }
            match self.resolve_intermediate_ref(inter_data) {
                Ok(relation) => new_igr.relation = relation,
                Err(error) => self.ctx.error(error),
            }
        } else if let Some((references_to, implicit)) =
            self.resolve_reference(&mut new_igr, location, located_ingredient.modifiers.span())
        {
            assert!(ingredient.intermediate_data.is_none()); // now unreachable, but just to be safe in the future

            let definition = &self.content.ingredients[references_to];
            let definition_location = &self.locations.ingredients[references_to];
            assert!(definition.relation.is_definition());

            if self.extensions.contains(Extensions::ADVANCED_UNITS) {
                if let Some(new_quantity) = &new_igr.quantity {
                    let all_quantities = std::iter::once(references_to)
                        .chain(definition.relation.referenced_from().iter().copied())
                        .filter_map(|index| {
                            self.content.ingredients[index]
                                .quantity
                                .as_ref()
                                .map(|q| (index, q))
                        });
                    for (index, q) in all_quantities {
                        if let Err(e) = q.compatible_unit(new_quantity, self.converter) {
                            let old_q_loc =
                                self.locations.ingredients[index].quantity.as_ref().unwrap();
                            let old = old_q_loc
                                .unit
                                .as_ref()
                                .map(|l| l.span())
                                .unwrap_or(old_q_loc.span());
                            let new_q_loc = located_ingredient.quantity.as_ref().unwrap();
                            let new = new_q_loc
                                .unit
                                .as_ref()
                                .map(|l| l.span())
                                .unwrap_or(new_q_loc.span());

                            let (new_label, old_label) = match &e {
                                crate::quantity::IncompatibleUnits::MissingUnit { found } => {
                                    let m = "this is missing a unit";
                                    let f = "matching this one";
                                    match found {
                                        either::Either::Left(_) => (label!(new, m), label!(old, f)),
                                        either::Either::Right(_) => (label!(new, f), label!(old, m)),
                                    }
                                }
                                crate::quantity::IncompatibleUnits::DifferentPhysicalQuantities {
                                    a: a_q,
                                    b: b_q,
                                } => {
                                    (label!(new, b_q.to_string()), label!(old, a_q.to_string()))
                                }
                                crate::quantity::IncompatibleUnits::UnknownDifferentUnits { .. } => {
                                    (label!(new, "differs from this"), label!(old, "this unit"))
                                }
                            };

                            self.ctx.warn(
                                warning!(
                                    "Incompatible units prevent calculating total amount",
                                    new_label
                                )
                                .label(old_label)
                                .set_source(e),
                            )
                        }
                    }
                }
            }

            if let Some(note) = &located_ingredient.note {
                self.ctx.error(note_reference_error(note.span(), implicit));
            }

            // When the ingredient is not defined in a step, only the definition
            // or the references can have quantities.
            // This is to avoid confusion when calculating the total amount.
            //  - If the user defines the ingredient in a ingredient list with
            //    a quantity and later references it with a quantity, what does
            //    the definition quantity mean? total? partial and the reference
            //    a portion used? Too messy. This situation is prohibited
            //  - If the user defines the ingredient directly in a step, it's
            //    quantity is used there, and the total is the sum of itself and
            //    all of its references. All clear.
            if definition.quantity.is_some()
                && new_igr.quantity.is_some()
                && !definition
                    .relation
                    .is_defined_in_step()
                    .expect("definition")
            {
                self.ctx.error(conflicting_reference_quantity_error(
                    ingredient.quantity.unwrap().span(),
                    definition_location.span(),
                    implicit,
                ));
            }

            // text value warning
            if let Some((ref_q, def_q)) =
                &new_igr.quantity.as_ref().zip(definition.quantity.as_ref())
            {
                let ref_is_text = ref_q.value.is_text();
                let def_is_text = def_q.value.is_text();

                if ref_is_text != def_is_text {
                    let ref_q_loc = located_ingredient.quantity.as_ref().unwrap().span();
                    let def_q_loc = definition_location.quantity.as_ref().unwrap().span();

                    let (text_quantity_span, number_quantity_span) = if ref_is_text {
                        (ref_q_loc, def_q_loc)
                    } else {
                        (def_q_loc, ref_q_loc)
                    };

                    self.ctx.warn(text_val_in_ref_warn(
                        text_quantity_span,
                        number_quantity_span,
                        implicit,
                    ));
                }
            }

            Ingredient::set_referenced_from(&mut self.content.ingredients, references_to);
        }

        if new_igr.modifiers.contains(Modifiers::RECIPE)
            && !new_igr.modifiers.contains(Modifiers::REF)
        {
            if let Some(checker) = &self.recipe_ref_checker {
                let res = (*checker)(&new_igr.name);

                if let RecipeRefCheckResult::NotFound { hints } = res {
                    let mut w = warning!(
                        format!("Referenced recipe not found: {}", new_igr.name),
                        label!(location)
                    );
                    for hint in hints {
                        w.add_hint(hint);
                    }
                    self.ctx.warn(w)
                }
            }
        }

        self.locations.ingredients.push(located_ingredient);
        self.content.ingredients.push(new_igr);
        self.content.ingredients.len() - 1
    }

    fn resolve_intermediate_ref(
        &mut self,
        inter_data: Located<IntermediateData>,
    ) -> Result<IngredientRelation, SourceDiag> {
        use ast::IntermediateRefMode::*;
        use ast::IntermediateTargetKind::*;
        assert!(!inter_data.val.is_negative());
        let val = inter_data.val as u32;

        const INVALID: &str = "Invalid intermediate preparation reference";

        if val == 0 {
            match inter_data.ref_mode {
                Number => {
                    return Err(error!(
                        format!("{INVALID}: number is 0"),
                        label!(inter_data.span())
                    )
                    .hint("Step and section numbers start at 1"));
                }
                Relative => {
                    return Err(error!(
                        format!("{INVALID}: relative reference to self"),
                        label!(inter_data.span())
                    )
                    .hint("Relative reference value has to be greater than 0"));
                }
            }
        }

        let bounds = |help: String| {
            Err(error!(
                format!("{INVALID}: value out of bounds"),
                label!(inter_data.span())
            )
            .hint(help))
        };

        let relation = match (inter_data.target_kind, inter_data.ref_mode) {
            (Step, Number) => {
                let index = self
                    .current_section
                    .content
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| c.is_step().then_some(i))
                    .nth((val - 1) as usize);

                if index.is_none() {
                    return bounds(
                        format!(
                            "The value has to be a previous step number: {}",
                            // -1 because step_counter holds the current step number
                            match self.step_counter.saturating_sub(1) {
                                0 => "no steps before this one".to_string(),
                                1 => "1".to_string(),
                                max => format!("1 to {max}"),
                            }
                        )
                        .into(),
                    );
                }

                IngredientRelation::reference(index.unwrap(), IngredientReferenceTarget::Step)
            }
            (Step, Relative) => {
                let index = self
                    .current_section
                    .content
                    .iter()
                    .enumerate()
                    .filter_map(|(i, c)| c.is_step().then_some(i))
                    .nth_back((val - 1) as usize);
                if index.is_none() {
                    return bounds(
                        format!(
                            "The current section {} steps before this one",
                            match self.step_counter.saturating_sub(1) {
                                0 => "has no".to_string(),
                                before => format!("only has {before}"),
                            }
                        )
                        .into(),
                    );
                }

                IngredientRelation::reference(index.unwrap(), IngredientReferenceTarget::Step)
            }
            (Section, Number) => {
                let index = (val - 1) as usize; // direct index, but make it 0 indexed

                if index >= self.content.sections.len() {
                    return bounds(
                        format!(
                            "The value has to be a previous section number: {}",
                            match self.content.sections.len() {
                                0 => "no sections before this one".to_string(),
                                1 => "1".to_string(),
                                max => format!("1 to {max}"),
                            }
                        )
                        .into(),
                    );
                }

                IngredientRelation::reference(index, IngredientReferenceTarget::Section)
            }
            (Section, Relative) => {
                let val = val as usize; // number of sections to go back

                // content.sections holds the past sections
                if val > self.content.sections.len() {
                    return bounds(
                        format!(
                            "The recipe {} sections before this one",
                            match self.content.sections.len() {
                                0 => "has no".to_string(),
                                before => format!("only has {before}"),
                            }
                        )
                        .into(),
                    );
                }

                // number of past sections - number to go back
                // val is at least 1, so the first posibility is the prev section index
                // val is checked to be smaller or equal, if equal, get 0, the index
                let index = self.content.sections.len().saturating_sub(val);
                IngredientRelation::reference(index, IngredientReferenceTarget::Section)
            }
        };
        Ok(relation)
    }

    fn cookware(&mut self, cookware: Located<ast::Cookware<'i>>) -> usize {
        let located_cookware = cookware.clone();
        let (cookware, location) = cookware.take_pair();

        let mut new_cw = Cookware {
            name: cookware.name.text_trimmed().into_owned(),
            alias: cookware.alias.map(|t| t.text_trimmed().into_owned()),
            quantity: cookware.quantity.map(|q| self.value(q.into_inner(), false)),
            note: cookware.note.map(|n| n.text_trimmed().into_owned()),
            modifiers: cookware.modifiers.into_inner(),
            relation: ComponentRelation::Definition {
                referenced_from: Vec::new(),
                defined_in_step: self.define_mode != DefineMode::Components,
            },
        };

        if let Some((references_to, implicit)) =
            self.resolve_reference(&mut new_cw, location, located_cookware.modifiers.span())
        {
            let definition = &self.content.cookware[references_to];
            let definition_location = &self.locations.cookware[references_to];
            assert!(definition.relation.is_definition());

            if let Some(note) = &located_cookware.note {
                self.ctx.error(note_reference_error(note.span(), implicit));
            }

            // See ingredients for explanation
            if definition.quantity.is_some()
                && new_cw.quantity.is_some()
                && !definition
                    .relation
                    .is_defined_in_step()
                    .expect("definition")
            {
                self.ctx.error(conflicting_reference_quantity_error(
                    located_cookware.quantity.as_ref().unwrap().span(),
                    definition_location.span(),
                    implicit,
                ));
            }

            // text value warning
            if let Some((ref_q, def_q)) =
                &new_cw.quantity.as_ref().zip(definition.quantity.as_ref())
            {
                let ref_is_text = ref_q.is_text();
                let def_is_text = def_q.is_text();

                if ref_is_text != def_is_text {
                    let ref_q_loc = located_cookware.quantity.as_ref().unwrap().span();
                    let def_q_loc = definition_location.quantity.as_ref().unwrap().span();

                    let (text_quantity_span, number_quantity_span) = if ref_is_text {
                        (ref_q_loc, def_q_loc)
                    } else {
                        (def_q_loc, ref_q_loc)
                    };

                    self.ctx.warn(text_val_in_ref_warn(
                        text_quantity_span,
                        number_quantity_span,
                        implicit,
                    ));
                }
            }

            Cookware::set_referenced_from(&mut self.content.cookware, references_to);
        }

        self.locations.cookware.push(located_cookware);
        self.content.cookware.push(new_cw);
        self.content.cookware.len() - 1
    }

    fn timer(&mut self, timer: Located<ast::Timer<'i>>) -> usize {
        let located_timer = timer.clone();
        let (timer, _span) = timer.take_pair();
        let quantity = timer.quantity.map(|q| {
            let quantity = self.quantity(q, false);
            if self.extensions.contains(Extensions::ADVANCED_UNITS) {
                let located_quantity = located_timer.quantity.as_ref().unwrap();
                if quantity.value.is_text() {
                    self.ctx.error(
                        error!(
                            format!("Timer value is text: {}", quantity.value),
                            label!(located_quantity.value.span(), "this should be numeric")
                        )
                        .hint("A timer duration cannot be text"),
                    );
                }
                if let Some(unit) = quantity.unit() {
                    let unit_span = located_quantity.unit.as_ref().unwrap().span();
                    match unit.unit_info_or_parse(self.converter) {
                        UnitInfo::Known(unit) => {
                            if unit.physical_quantity != PhysicalQuantity::Time {
                                self.ctx.error(error!(
                                    format!("Timer unit is not time: {unit}"),
                                    label!(
                                        unit_span,
                                        "expected time, not {}",
                                        unit.physical_quantity
                                    )
                                ));
                            }
                        }
                        UnitInfo::Unknown => self.ctx.error(error!(
                            format!("Unknown timer unit: {unit}"),
                            label!(unit_span, "expected time unit")
                        )),
                    }
                }
            }
            quantity
        });

        let new_timer = Timer {
            name: timer.name.map(|t| t.text_trimmed().into_owned()),
            quantity,
        };

        self.content.timers.push(new_timer);
        self.content.timers.len() - 1
    }

    fn quantity(
        &mut self,
        quantity: Located<ast::Quantity<'i>>,
        is_ingredient: bool,
    ) -> Quantity<ScalableValue> {
        let ast::Quantity { value, unit, .. } = quantity.into_inner();
        Quantity::new(
            self.value(value, is_ingredient),
            unit.map(|t| t.text_trimmed().into_owned()),
        )
    }

    fn value(&mut self, value: ast::QuantityValue, is_ingredient: bool) -> ScalableValue {
        let mut marker_span = None;
        match &value {
            ast::QuantityValue::Single {
                value,
                auto_scale: Some(auto_scale_marker),
            } => {
                marker_span = Some(*auto_scale_marker);
                if value.is_text() {
                    self.ctx.error(
                        error!(
                            "Text value with auto scale marker",
                            label!(auto_scale_marker, "remove this")
                        )
                        .hint("Text cannot be scaled"),
                    );
                }
            }
            ast::QuantityValue::Many(v) => {
                const CONFLICT: &str = "Many values conflict";
                if let Some(s) = &self.content.metadata.servings {
                    let servings_meta_span = self
                        .locations
                        .metadata
                        .get("servings")
                        .map(|(_, value)| value.span())
                        .unwrap();
                    if s.len() != v.len() {
                        self.ctx.error(
                            error!(
                                format!(
                                    "{CONFLICT}: {} servings defined but {} values in the quantity",
                                    s.len(),
                                    v.len()
                                ),
                                label!(value.span(), "number of values do not match servings")
                            )
                            .label(label!(servings_meta_span, "servings defined here")),
                        );
                    }
                } else {
                    self.ctx.error(error!(
                        format!(
                            "{CONFLICT}: not servings defined but {} values in the quantity",
                            v.len()
                        ),
                        label!(value.span())
                    ));
                }
            }
            _ => {}
        }
        let mut v = ScalableValue::from_ast(value);

        if is_ingredient && self.auto_scale_ingredients {
            match v {
                ScalableValue::Fixed(value) if !value.is_text() => v = ScalableValue::Linear(value),
                ScalableValue::Linear(_) => {
                    self.ctx.warn(
                        warning!(
                            "Redundant auto scale marker",
                            label!(marker_span.unwrap(), "remove this")
                        )
                        .hint("Every ingredient is already marked to auto scale"),
                    );
                }
                _ => {}
            };
        }

        v
    }

    fn resolve_reference<C: RefComponent>(
        &mut self,
        new: &mut C,
        location: Span,
        modifiers_location: Span,
    ) -> Option<(usize, bool)> {
        let all = C::all(&self.content);
        let new_name = new.name().to_lowercase();
        // find the LAST component with the same name, lazy
        let same_name_cell = std::cell::OnceCell::new();
        let same_name = || {
            *same_name_cell.get_or_init(|| {
                C::all(&self.content).iter().rposition(|other: &C| {
                    !other.modifiers().contains(Modifiers::REF)
                        && new_name == other.name().to_lowercase()
                })
            })
        };

        let conflicing_modifiers = |conflict: Modifiers, help: CowStr, implicit: bool| {
            let mut e = error!(
                format!("Unsupported modifier combination with reference: {conflict}"),
                label!(modifiers_location)
            )
            .hint(help);
            if implicit {
                e.add_hint(IMPLICIT_REF_WARN);
            }
            e
        };

        let redundant_modifier = |redundant: &'static str, help: String| {
            warning!(
                format!("Redundant {redundant} modifier"),
                label!(modifiers_location)
            )
            .hint(help)
            .hint(format!(
                "In the current mode, by default, {}",
                match (self.define_mode, self.duplicate_mode) {
                    (DefineMode::Steps, _) => "all components are references",
                    (_, DuplicateMode::Reference) =>
                        "components are definitions but duplicates are references",
                    _ => "all components are definitions",
                }
            ))
        };

        // no new and ref -> error
        if new.modifiers().contains(Modifiers::NEW | Modifiers::REF) {
            self.ctx.error(conflicing_modifiers(
                *new.modifiers(),
                "NEW (+) can never be combined with REF (&)".into(),
                false,
            ));
            return None;
        }

        // no new -> maybe warning for redundant
        if new.modifiers().contains(Modifiers::NEW) {
            if self.define_mode != DefineMode::Steps {
                if self.duplicate_mode == DuplicateMode::Reference && same_name().is_none() {
                    self.ctx.warn(redundant_modifier(
                        "new (+)",
                        format!("There are no {}s with the same name before", C::container()),
                    ));
                } else if self.duplicate_mode == DuplicateMode::New {
                    self.ctx.warn(redundant_modifier(
                        "new (+)",
                        format!("This {} is already a definition", C::container()),
                    ));
                }
            }
            return None;
        }

        // warning for redundant ref
        if (self.duplicate_mode == DuplicateMode::Reference
            || self.define_mode == DefineMode::Steps)
            && new.modifiers().contains(Modifiers::REF)
        {
            self.ctx.warn(redundant_modifier(
                "reference (&)",
                format!("This {} is already a reference", C::container()),
            ));
        }

        let treat_as_reference = new.modifiers().contains(Modifiers::REF)
            || self.define_mode == DefineMode::Steps
            || self.duplicate_mode == DuplicateMode::Reference && same_name().is_some();

        if !treat_as_reference {
            return None;
        }

        // the reference is implicit if we are here (is a reference) and the
        // reference modifier is not set
        let implicit = !new.modifiers().contains(Modifiers::REF);

        if let Some(references_to) = same_name() {
            let referenced = &all[references_to];
            assert!(!referenced.modifiers().contains(Modifiers::REF));

            // Set of inherited modifiers from the definition
            let inherited = *referenced.modifiers() & C::inherit_modifiers();
            // Set of conflict modifiers
            //   - any modifiers not inherited
            //   - is not ref
            // except ref and new, the only modifiers a reference can have is those inherited
            // from the definition. And if it has it's not treated as a reference.
            let conflict = *new.modifiers() & !inherited & !Modifiers::REF;

            // Apply inherited
            *new.modifiers_mut() |= inherited;

            // Set it as a reference
            *new.modifiers_mut() |= Modifiers::REF;
            new.set_reference(references_to);

            if !conflict.is_empty() {
                let help = {
                    let extra = conflict
                        .iter_names()
                        .map(|(s, _)| s.to_lowercase())
                        .collect::<Vec<_>>()
                        .join(", ");
                    if implicit {
                        format!("Mark the definition as {extra} or add new (+) to this")
                    } else {
                        format!("Mark the definition as {extra} or remove the reference (&)")
                    }
                };
                self.ctx
                    .error(conflicing_modifiers(conflict, help.into(), implicit));
            }

            // extra reference checks
            Some((references_to, implicit))
        } else {
            self.ctx.error({
                let mut e = error!(
                    format!("Reference not found: {}", new.name()),
                    label!(location)
                )
                .hint(format!(
                    "A non reference {} with the same name defined BEFORE cannot be found",
                    C::container()
                ));
                if implicit {
                    e.add_hint(IMPLICIT_REF_WARN);
                }
                e
            });
            None
        }
    }
}

trait RefComponent: Sized {
    fn name(&self) -> &str;
    fn modifiers(&self) -> &Modifiers;
    fn modifiers_mut(&mut self) -> &mut Modifiers;

    fn inherit_modifiers() -> Modifiers;

    fn container() -> &'static str;

    fn set_reference(&mut self, references_to: usize);
    fn set_referenced_from(all: &mut [Self], references_to: usize);

    fn all(content: &ScalableRecipe) -> &[Self];
}

impl RefComponent for Ingredient<ScalableValue> {
    #[inline]
    fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    fn modifiers(&self) -> &Modifiers {
        &self.modifiers
    }

    #[inline]
    fn modifiers_mut(&mut self) -> &mut Modifiers {
        &mut self.modifiers
    }

    #[inline]
    fn inherit_modifiers() -> Modifiers {
        Modifiers::HIDDEN | Modifiers::OPT | Modifiers::RECIPE
    }

    #[inline]
    fn container() -> &'static str {
        "ingredient"
    }

    #[inline]
    fn set_reference(&mut self, references_to: usize) {
        self.relation =
            IngredientRelation::reference(references_to, IngredientReferenceTarget::Ingredient);
    }

    fn set_referenced_from(all: &mut [Self], references_to: usize) {
        let new_index = all.len();
        match all[references_to].relation.referenced_from_mut() {
            Some(referenced_from) => {
                referenced_from.push(new_index);
            }
            None => panic!("Reference to reference"),
        }
    }

    #[inline]
    fn all(content: &ScalableRecipe) -> &[Self] {
        &content.ingredients
    }
}

impl RefComponent for Cookware<ScalableValue> {
    #[inline]
    fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    fn modifiers(&self) -> &Modifiers {
        &self.modifiers
    }

    #[inline]
    fn modifiers_mut(&mut self) -> &mut Modifiers {
        &mut self.modifiers
    }

    #[inline]
    fn inherit_modifiers() -> Modifiers {
        Modifiers::HIDDEN | Modifiers::OPT
    }

    #[inline]
    fn container() -> &'static str {
        "cookware item"
    }

    #[inline]
    fn set_reference(&mut self, references_to: usize) {
        self.relation = ComponentRelation::Reference { references_to };
    }

    fn set_referenced_from(all: &mut [Self], references_to: usize) {
        let new_index = all.len();
        match &mut all[references_to].relation {
            ComponentRelation::Definition {
                referenced_from, ..
            } => referenced_from.push(new_index),
            ComponentRelation::Reference { .. } => panic!("Reference to reference"),
        }
    }

    #[inline]
    fn all(content: &ScalableRecipe) -> &[Self] {
        &content.cookware
    }
}

fn find_temperature<'a>(text: &'a str, re: &Regex) -> Option<(&'a str, Quantity<Value>, &'a str)> {
    let Some(caps) = re.captures(text) else {
        return None;
    };

    let value = caps[1].replace(',', ".").parse::<f64>().ok()?;
    let unit = caps.get(3).unwrap().range();
    let unit_text = text[unit].to_string();
    let temperature = Quantity::new(Value::Number(value.into()), Some(unit_text));

    let range = caps.get(0).unwrap().range();
    let (before, after) = (&text[..range.start], &text[range.end..]);

    Some((before, temperature, after))
}

fn note_reference_error(span: Span, implicit: bool) -> SourceDiag {
    let mut e = error!("Note not allowed in reference", label!(span, "remove this"))
        .hint("Add the note in the definition of the ingredient");
    if implicit {
        e.add_hint(IMPLICIT_REF_WARN);
    }
    e
}

fn conflicting_reference_quantity_error(
    ref_quantity_span: Span,
    def_span: Span,
    implicit: bool,
) -> SourceDiag {
    let mut e = error!(
        "Conflicting component reference quantities",
        label!(ref_quantity_span, "remove this")
    )
    .label(label!(
        def_span,
        "definition with quantity outside a step here"
    ))
    .hint("If the component is not defined in a step and has a quantity, its references cannot have a quantity");
    if implicit {
        e.add_hint(IMPLICIT_REF_WARN);
    }
    e
}

fn text_val_in_ref_warn(
    text_quantity_span: Span,
    number_quantity_span: Span,
    implicit: bool,
) -> SourceDiag {
    let mut w = warning!(
        "Text value may prevent calculating total amount",
        label!(text_quantity_span, "this text")
    )
    .label(label!(
        number_quantity_span,
        "cannot be added to this value"
    ))
    .hint("Use numeric values so they can be added together");
    if implicit {
        w.add_hint(IMPLICIT_REF_WARN);
    }
    w
}
