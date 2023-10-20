use std::sync::Arc;

use cooklang::aisle::parse as parse_aisle_config_original;
use cooklang::analysis::parse_events;
use cooklang::parser::PullParser;
use cooklang::Extensions;
use cooklang::{Converter, ScalableRecipe};

pub mod aisle;
pub mod model;

use aisle::*;
use model::*;

fn simplify_recipe_data(recipe: &ScalableRecipe) -> CooklangRecipe {
    let mut metadata = CooklangMetadata::new();
    let mut steps: Vec<Step> = Vec::new();
    let mut ingredients: IngredientList = IngredientList::default();
    let mut cookware: Vec<Item> = Vec::new();
    let mut items: Vec<Item> = Vec::new();

    recipe.sections.iter().for_each(|section| {
        section.content.iter().for_each(|content| {
            if let cooklang::Content::Step(step) = content {
                step.items.iter().for_each(|item| {
                    let i = into_item(item.clone(), recipe);

                    match i {
                        Item::Ingredient {
                            ref name,
                            ref amount,
                        } => {
                            let quantity = into_group_quantity(amount);
                            add_to_ingredient_list(&mut ingredients, name, &quantity);
                        }
                        Item::Cookware { .. } => {
                            cookware.push(i.clone());
                        }
                        // don't need anything if timer or text
                        _ => (),
                    };
                    items.push(i);
                });
                // TODO: think how to make it faster as we probably
                // can switch items content directly into the step object without cloning it
                steps.push(Step {
                    items: items.clone(),
                });

                items.clear();
            }
        });
    });

    recipe.metadata.map.iter().for_each(|(key, value)| {
        metadata.insert(key.to_string(), value.to_string());
    });

    CooklangRecipe {
        metadata,
        steps,
        ingredients,
        cookware,
    }
}

#[uniffi::export]
pub fn parse_recipe(input: String) -> CooklangRecipe {
    let extensions = Extensions::empty();
    let converter = Converter::empty();

    let mut parser = PullParser::new(&input, extensions);
    let parsed = parse_events(&mut parser, extensions, &converter, None)
        .take_output()
        .unwrap();

    simplify_recipe_data(&parsed)
}

#[uniffi::export]
pub fn parse_metadata(input: String) -> CooklangMetadata {
    let mut metadata = CooklangMetadata::new();
    let extensions = Extensions::empty();
    let converter = Converter::empty();

    let parser = PullParser::new(&input, extensions);

    let parsed = parse_events(parser.into_meta_iter(), extensions, &converter, None)
        .map(|c| c.metadata.map)
        .take_output()
        .unwrap();

    let _ = &(parsed).iter().for_each(|(key, value)| {
        metadata.insert(key.to_string(), value.to_string());
    });

    metadata
}

#[uniffi::export]
pub fn parse_aisle_config(input: String) -> Arc<AisleConf> {
    let mut categories: Vec<AisleCategory> = Vec::new();
    let mut ingredients: Vec<AisleIngredient> = Vec::new();
    let mut cache: AisleReverseCategory = AisleReverseCategory::default();

    let parsed = parse_aisle_config_original(&input).unwrap();

    let _ = &(parsed).categories.iter().for_each(|c| {
        let category_name = c.name.to_string();

        c.ingredients.iter().for_each(|i| {
            let mut it = i.names.iter();

            let name = it.next().unwrap().to_string();
            let aliases: Vec<String> = it.map(|v| v.to_string()).collect();

            cache.insert(name.clone(), category_name.clone());
            aliases.iter().for_each(|a| {
                cache.insert(a.to_string(), category_name.clone());
            });

            ingredients.push(AisleIngredient { name, aliases });
        });

        let category = AisleCategory {
            name: category_name,
            ingredients: ingredients.clone(),
        };

        ingredients.clear();
        categories.push(category);
    });

    let config = AisleConf { categories, cache };

    Arc::new(config)
}

#[uniffi::export]
pub fn combine_ingredients(lists: Vec<IngredientList>) -> IngredientList {
    let mut combined: IngredientList = IngredientList::default();

    lists.iter().for_each(|l| {
        merge_ingredient_lists(&mut combined, l);
    });

    combined
}

uniffi::setup_scaffolding!();

#[cfg(test)]
mod tests {

    #[test]
    fn test_parse_recipe() {
        use crate::{parse_recipe, Amount, Item, Value};

        let recipe = parse_recipe(
            r#"
a test @step @salt{1%mg} more text
"#
            .to_string(),
        );

        assert_eq!(
            recipe.steps.into_iter().nth(0).unwrap().items,
            vec![
                Item::Text {
                    value: "a test ".to_string()
                },
                Item::Ingredient {
                    name: "step".to_string(),
                    amount: None
                },
                Item::Text {
                    value: " ".to_string()
                },
                Item::Ingredient {
                    name: "salt".to_string(),
                    amount: Some(Amount {
                        quantity: Value::Number { value: 1.0 },
                        units: Some("mg".to_string())
                    })
                },
                Item::Text {
                    value: " more text".to_string()
                }
            ]
        );
    }

    #[test]
    fn test_parse_metadata() {
        use crate::parse_metadata;
        use std::collections::HashMap;

        let metadata = parse_metadata(
            r#"
>> source: https://google.com
a test @step @salt{1%mg} more text
"#
            .to_string(),
        );

        assert_eq!(
            metadata,
            HashMap::from([("source".to_string(), "https://google.com".to_string())])
        );
    }

    #[test]
    fn test_parse_aisle_config() {
        use crate::parse_aisle_config;
        use std::collections::HashMap;

        let config = parse_aisle_config(
            r#"
[fruit and veg]
apple gala | apples
aubergine
avocado | avocados

[milk and dairy]
butter
egg | eggs
curd cheese
cheddar cheese
feta

[dried herbs and spices]
bay leaves
black pepper
cayenne pepper
dried oregano
"#
            .to_string(),
        );

        assert_eq!(
            config.category_for("bay leaves".to_string()),
            Some("dried herbs and spices".to_string())
        );

        assert_eq!(
            config.category_for("eggs".to_string()),
            Some("milk and dairy".to_string())
        );

        assert_eq!(
            config.category_for("some weird ingredient".to_string()),
            None
        );
    }

    #[test]
    fn test_combine_ingredients() {
        use crate::{combine_ingredients, HardToNameWTF, QuantityType, Value};
        use std::collections::HashMap;

        let combined = combine_ingredients(
            vec![
                HashMap::from([
                    (
                        "salt".to_string(),
                        HashMap::from([
                            (HardToNameWTF { name: "g".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 5.0 }),
                            (HardToNameWTF { name: "tsp".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 1.0 }),
                        ])
                    ),
                    (
                        "pepper".to_string(),
                        HashMap::from([
                            (HardToNameWTF { name: "mg".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 5.0 }),
                            (HardToNameWTF { name: "tsp".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 1.0 }),
                        ])
                    ),
                ]),
                HashMap::from([
                    (
                        "salt".to_string(),
                        HashMap::from([
                            (HardToNameWTF { name: "kg".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 0.005 }),
                            (HardToNameWTF { name: "tsp".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 1.0 }),
                        ])
                    ),
                ])
            ]
        );

        assert_eq!(
            *combined.get("salt").unwrap(),
            HashMap::from([
                (HardToNameWTF { name: "kg".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 0.005 }),
                (HardToNameWTF { name: "tsp".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 2.0 }),
                (HardToNameWTF { name: "g".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 5.0 }),
            ])
        );

        assert_eq!(
            *combined.get("pepper").unwrap(),
            HashMap::from([
                (HardToNameWTF { name: "mg".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 5.0 }),
                (HardToNameWTF { name: "tsp".to_string(), unit_type: QuantityType::Number }, Value::Number { value: 1.0 }),
            ])
        );
    }
}
