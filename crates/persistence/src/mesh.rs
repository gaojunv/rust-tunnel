use chrono::Utc;

use super::records::{MeshNetworkRecord, MeshServiceRecord};
use super::Database;

impl Database {
    /// Save a mesh network record
    ///
    /// # Errors
    /// 当数据库写入或连接失败时返回 `sqlx::Error`。
    pub async fn save_mesh_network(
        &self,
        id: &str,
        description: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let now = Utc::now();
        sqlx::query(
            r"
            INSERT INTO mesh_networks (id, created_at, description)
            VALUES (?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET description = excluded.description
            ",
        )
        .bind(id)
        .bind(now)
        .bind(description)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load all mesh networks
    ///
    /// # Errors
    /// 当数据库查询执行失败时返回 `sqlx::Error`。
    pub async fn load_mesh_networks(&self) -> Result<Vec<MeshNetworkRecord>, sqlx::Error> {
        sqlx::query_as::<_, MeshNetworkRecord>(
            "SELECT id, created_at, description FROM mesh_networks ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Save or update a mesh service
    ///
    /// # Errors
    /// 当数据库写入或连接失败时返回 `sqlx::Error`。
    pub async fn save_mesh_service(
        &self,
        mesh_id: &str,
        client_name: &str,
        service_name: &str,
        protocol: &str,
        local_addr: &str,
        dns_record: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r"
            INSERT INTO mesh_services (mesh_id, client_name, service_name, protocol, local_addr, dns_record)
            VALUES (?, ?, ?, ?, ?, ?)
            ON CONFLICT(mesh_id, service_name) DO UPDATE SET
                client_name = excluded.client_name,
                protocol = excluded.protocol,
                local_addr = excluded.local_addr,
                dns_record = excluded.dns_record
            ",
        )
        .bind(mesh_id)
        .bind(client_name)
        .bind(service_name)
        .bind(protocol)
        .bind(local_addr)
        .bind(dns_record)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load services for a mesh
    ///
    /// # Errors
    /// 当数据库查询执行失败时返回 `sqlx::Error`。
    pub async fn load_mesh_services(
        &self,
        mesh_id: &str,
    ) -> Result<Vec<MeshServiceRecord>, sqlx::Error> {
        sqlx::query_as::<_, MeshServiceRecord>(
            "SELECT id, mesh_id, client_name, service_name, protocol, local_addr, dns_record \
             FROM mesh_services WHERE mesh_id = ? ORDER BY service_name",
        )
        .bind(mesh_id)
        .fetch_all(&self.pool)
        .await
    }

    /// Delete a mesh service
    ///
    /// # Errors
    /// 当数据库删除执行失败时返回 `sqlx::Error`。
    pub async fn delete_mesh_service(
        &self,
        mesh_id: &str,
        service_name: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM mesh_services WHERE mesh_id = ? AND service_name = ?")
            .bind(mesh_id)
            .bind(service_name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
