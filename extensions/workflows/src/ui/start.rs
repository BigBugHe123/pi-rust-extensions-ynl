//! Starting a run from the UI: models marked `ask`, then the inputs.

use std::collections::BTreeMap;

use pi_ext_sdk::prelude::*;

use crate::model::{ASK, Mode, Workflow};
use crate::runs::Origin;
use crate::ui::fields::choose_model;
use crate::ui::{Screen, Ui};

/// A run that started, and the line that says how it runs.
pub struct Started {
    pub id: String,
    pub message: String,
}

/// `Ok(None)` when the user stops at a dialog; `Err` names why the run cannot start.
pub async fn run_dialog(
    ui: &Ui,
    screen: &mut Screen,
    mut workflow: Workflow,
) -> Result<std::result::Result<Option<Started>, String>> {
    let name = workflow.name.clone();
    let mut asks: Vec<(String, &'static str, &mut Option<String>)> = Vec::new();
    asks.push((
        format!("Default model for the steps of {name}"),
        "the session model",
        &mut workflow.agent.model,
    ));
    if workflow.mode == Mode::Dynamic {
        asks.push((
            format!("Planner model of {name}"),
            "the session model",
            &mut workflow.planner.model,
        ));
    }
    for step in &mut workflow.steps {
        asks.push((
            format!("Model for step {} of {name}", step.name),
            "the workflow default or the session model",
            &mut step.agent.model,
        ));
    }
    for (title, inherit, model) in asks {
        if model.as_deref() != Some(ASK) {
            continue;
        }
        match choose_model(ui, screen, &title, false, inherit).await? {
            Some(Ok(chosen)) => *model = chosen,
            Some(Err(err)) => return Ok(Err(err)),
            None => return Ok(Ok(None)),
        }
    }
    let mut inputs = BTreeMap::new();
    for input in workflow.inputs() {
        let title = if input == "goal" {
            format!("Goal of {name}: what must the workflow reach?")
        } else {
            format!("{input} ({name})")
        };
        let Some(value) = ui.host.editor(&title, None).await? else {
            return Ok(Ok(None));
        };
        if value.trim().is_empty() {
            return Ok(Err(format!("The run needs a value for {input}.")));
        }
        inputs.insert(input, value.trim().to_owned());
    }
    let background = workflow.background;
    let started = ui
        .app
        .start(&ui.host, workflow, inputs, Origin::User, background)
        .await;
    Ok(match started {
        Ok(id) => {
            let mode = if background {
                "in the background; it keeps going after pi quits"
            } else {
                "in the foreground; it stops when pi quits"
            };
            Ok(Some(Started {
                message: format!("Started {name} ({id}) {mode}."),
                id,
            }))
        }
        Err(err) => Err(format!("Cannot start {name}: {err}")),
    })
}
