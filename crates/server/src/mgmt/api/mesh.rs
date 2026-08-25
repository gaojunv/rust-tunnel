//! Mesh 网络管理 API。

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

use super::{
    dto::{MeshMemberResponse, MeshNetworkResponse, MeshServiceResponse},
    ApiState,
};

// ── Mesh Network Endpoints ─────────────────────────────────────────

// GET /api/mesh — list all meshes

/// `GET /api/mesh`：列出所有 Mesh 网络。
pub async fn list_meshes(State(state): State<ApiState>) -> impl IntoResponse {
    let networks = state.server_state.mesh_manager.list_networks().await;
    let response: Vec<MeshNetworkResponse> = networks
        .into_iter()
        .map(|(id, members)| {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();

            MeshNetworkResponse {
                id,
                members: members
                    .iter()
                    .map(|m| MeshMemberResponse {
                        client_name: m.client_name.clone(),
                        public_addr: m.public_addr.clone(),
                        p2p_available: m.p2p_available,
                        online: true,
                    })
                    .collect(),
                services,
            }
        })
        .collect();
    Json(response)
}

// GET /api/mesh/:id — mesh detail
/// `GET /api/mesh/:id`：查询单个 Mesh 详情。
pub async fn get_mesh(
    State(state): State<ApiState>,
    Path(mesh_id): Path<String>,
) -> impl IntoResponse {
    match state.server_state.mesh_manager.get_mesh(&mesh_id).await {
        Some(members) => {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();

            Json(MeshNetworkResponse {
                id: mesh_id,
                members: members
                    .iter()
                    .map(|m| MeshMemberResponse {
                        client_name: m.client_name.clone(),
                        public_addr: m.public_addr.clone(),
                        p2p_available: m.p2p_available,
                        online: true,
                    })
                    .collect(),
                services,
            })
            .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

// GET /api/mesh/:id/services — mesh services
/// `GET /api/mesh/:id/services`：列出指定 Mesh 的服务。
pub async fn get_mesh_services(
    State(state): State<ApiState>,
    Path(mesh_id): Path<String>,
) -> impl IntoResponse {
    match state.server_state.mesh_manager.get_mesh(&mesh_id).await {
        Some(members) => {
            let services: Vec<MeshServiceResponse> = members
                .iter()
                .flat_map(|m| {
                    m.services.iter().map(|s| MeshServiceResponse {
                        service_name: s.name.clone(),
                        protocol: s.protocol.clone(),
                        local_addr: s.local_addr.clone(),
                        client_name: m.client_name.clone(),
                    })
                })
                .collect();
            Json(services).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
