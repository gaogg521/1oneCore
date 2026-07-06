//! `/api/one/employee/*` routes. Mount behind the upstream auth middleware
//! (handlers read `CurrentUser` from request extensions; employees are
//! strictly owner-scoped in M3a).

use axum::extract::{Path, State};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};

use aionui_api_types::{ApiResponse, CronScheduleDto};
use aionui_auth::CurrentUser;

use crate::error::EmployeeError;
use crate::models::{EmployeeRunRow, PersonalAgentDto};
use crate::service::{CreateEmployeeInput, ScheduleInput, UpdateEmployeeInput};
use crate::state::OneEmployeeRouterState;

pub fn one_employee_routes(state: OneEmployeeRouterState) -> Router {
    Router::new()
        .route("/api/one/employee/agents", get(list_agents).post(create_agent))
        .route(
            "/api/one/employee/agents/{agent_id}",
            get(get_agent).put(update_agent).delete(delete_agent),
        )
        .route("/api/one/employee/agents/{agent_id}/run", post(run_agent))
        .route("/api/one/employee/agents/{agent_id}/run-team", post(run_agent_team))
        .route("/api/one/employee/agents/{agent_id}/schedule", put(set_schedule))
        .route("/api/one/employee/agents/{agent_id}/visibility", put(set_visibility))
        .route("/api/one/employee/agents/{agent_id}/runs", get(list_runs))
        .route("/api/one/employee/runs/{run_id}", get(get_run))
        .with_state(state)
}

async fn list_agents(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<PersonalAgentDto>>>, EmployeeError> {
    // Own employees plus any shared within the caller's tenant (A1 L3).
    let tenant = state.tenant_of(&user.id).await;
    Ok(Json(ApiResponse::ok(state.service.list_available(&user.id, &tenant).await?)))
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
    // Tenant is the caller's org tenant (personal edition → 'default'); it
    // determines who a later-shared employee reaches (A1 L3).
    let tenant = state.tenant_of(&user.id).await;
    let agent = state
        .service
        .create(&user.id, &tenant, CreateEmployeeInput {
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetVisibilityBody {
    visibility: String,
}

/// Share/unshare an employee within the owner's tenant. Owner-only.
async fn set_visibility(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(agent_id): Path<String>,
    Json(body): Json<SetVisibilityBody>,
) -> Result<Json<ApiResponse<PersonalAgentDto>>, EmployeeError> {
    let agent = state.service.set_visibility(&user.id, &agent_id, &body.visibility).await?;
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunTeamBody {
    team_id: String,
    slot_id: String,
}

async fn run_agent_team(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(agent_id): Path<String>,
    Json(body): Json<RunTeamBody>,
) -> Result<Json<ApiResponse<RunNowDto>>, EmployeeError> {
    let (run_id, conversation_id) = state
        .service
        .run_now_team(&user.id, &agent_id, &body.team_id, &body.slot_id)
        .await?;
    Ok(Json(ApiResponse::ok(RunNowDto { run_id, conversation_id })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetScheduleBody {
    schedule: Option<CronScheduleDto>,
    #[serde(default)]
    enabled: Option<bool>,
}

async fn set_schedule(
    State(state): State<OneEmployeeRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(agent_id): Path<String>,
    Json(body): Json<SetScheduleBody>,
) -> Result<Json<ApiResponse<PersonalAgentDto>>, EmployeeError> {
    let agent = state
        .service
        .set_schedule(&user.id, &agent_id, ScheduleInput {
            schedule: body.schedule,
            enabled: body.enabled,
        })
        .await?;
    Ok(Json(ApiResponse::ok(agent)))
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
