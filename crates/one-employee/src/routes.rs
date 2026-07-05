//! `/api/one/employee/*` routes. Mount behind the upstream auth middleware
//! (handlers read `CurrentUser` from request extensions; employees are
//! strictly owner-scoped in M3a).

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use aionui_api_types::ApiResponse;
use aionui_auth::CurrentUser;

use crate::error::EmployeeError;
use crate::models::{EmployeeRunRow, PersonalAgentDto};
use crate::service::{CreateEmployeeInput, UpdateEmployeeInput};
use crate::state::OneEmployeeRouterState;

pub fn one_employee_routes(state: OneEmployeeRouterState) -> Router {
    Router::new()
        .route("/api/one/employee/agents", get(list_agents).post(create_agent))
        .route(
            "/api/one/employee/agents/{agent_id}",
            get(get_agent).put(update_agent).delete(delete_agent),
        )
        .route("/api/one/employee/agents/{agent_id}/run", post(run_agent))
        .route("/api/one/employee/agents/{agent_id}/runs", get(list_runs))
        .route("/api/one/employee/runs/{run_id}", get(get_run))
        .with_state(state)
}

async fn list_agents(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<PersonalAgentDto>>>, EmployeeError> {
    Ok(Json(ApiResponse::ok(state.service.list(&user.id).await?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateAgentBody {
    name: String,
    description: Option<String>,
    agent_type: String,
    custom_agent_id: Option<String>,
    cli_path: Option<String>,
    automation_config: Option<serde_json::Value>,
}

async fn create_agent(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Json(body): Json<CreateAgentBody>,
) -> Result<Json<ApiResponse<PersonalAgentDto>>, EmployeeError> {
    // Tenant scoping follows one-org membership in M3b; personal edition
    // default is fine for M3a.
    let agent = state
        .service
        .create(&user.id, "default", CreateEmployeeInput {
            name: body.name,
            description: body.description,
            agent_type: body.agent_type,
            custom_agent_id: body.custom_agent_id,
            cli_path: body.cli_path,
            automation_config: body.automation_config,
        })
        .await?;
    Ok(Json(ApiResponse::ok(agent)))
}

async fn get_agent(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(agent_id): Path<String>,
) -> Result<Json<ApiResponse<PersonalAgentDto>>, EmployeeError> {
    Ok(Json(ApiResponse::ok(state.service.get(&user.id, &agent_id).await?.into())))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateAgentBody {
    name: Option<String>,
    description: Option<String>,
    automation_config: Option<serde_json::Value>,
}

async fn update_agent(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(agent_id): Path<String>,
    Json(body): Json<UpdateAgentBody>,
) -> Result<Json<ApiResponse<PersonalAgentDto>>, EmployeeError> {
    let agent = state
        .service
        .update(&user.id, &agent_id, UpdateEmployeeInput {
            name: body.name,
            description: body.description,
            automation_config: body.automation_config,
        })
        .await?;
    Ok(Json(ApiResponse::ok(agent)))
}

async fn delete_agent(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(agent_id): Path<String>,
) -> Result<Json<ApiResponse<()>>, EmployeeError> {
    state.service.delete(&user.id, &agent_id).await?;
    Ok(Json(ApiResponse::ok(())))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RunNowDto {
    run_id: String,
    conversation_id: String,
}

async fn run_agent(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(agent_id): Path<String>,
) -> Result<Json<ApiResponse<RunNowDto>>, EmployeeError> {
    let (run_id, conversation_id) = state.service.run_now(&user.id, &agent_id).await?;
    Ok(Json(ApiResponse::ok(RunNowDto { run_id, conversation_id })))
}

async fn list_runs(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(agent_id): Path<String>,
) -> Result<Json<ApiResponse<Vec<EmployeeRunRow>>>, EmployeeError> {
    Ok(Json(ApiResponse::ok(state.service.list_runs(&user.id, &agent_id).await?)))
}

async fn get_run(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(run_id): Path<String>,
) -> Result<Json<ApiResponse<EmployeeRunRow>>, EmployeeError> {
    Ok(Json(ApiResponse::ok(state.service.get_run(&user.id, &run_id).await?)))
}
