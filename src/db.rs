use scylla::Session;
use scylla::prepared_statement::PreparedStatement;
use std::sync::Arc;

pub struct ScyllaDb {
    pub session: Arc<Session>,
    pub get_unprocessed_concepts: PreparedStatement,
    pub mark_concept_processed: PreparedStatement,
    pub mark_concept_processed_by_owner: PreparedStatement,
    pub delete_concept: PreparedStatement,
    pub delete_concept_by_owner: PreparedStatement,
    pub delete_unprocessed_concept: PreparedStatement,
    pub get_concept: PreparedStatement,
}

impl ScyllaDb {
    pub async fn connect(nodes: &[String], keyspace: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let session = scylla::SessionBuilder::new()
            .known_nodes(nodes)
            .use_keyspace(keyspace, false)
            .build()
            .await?;
        let session = Arc::new(session);

        let get_unprocessed_concepts = session.prepare("SELECT id, type FROM unprocessed_concepts WHERE partition = 0").await?;
        let mark_concept_processed = session.prepare("UPDATE media_concepts SET processed = true WHERE id = ?").await?;
        let mark_concept_processed_by_owner = session.prepare("UPDATE media_concepts_by_owner SET processed = true WHERE owner = ? AND id = ?").await?;
        let delete_concept = session.prepare("DELETE FROM media_concepts WHERE id = ?").await?;
        let delete_concept_by_owner = session.prepare("DELETE FROM media_concepts_by_owner WHERE owner = ? AND id = ?").await?;
        let delete_unprocessed_concept = session.prepare("DELETE FROM unprocessed_concepts WHERE partition = 0 AND id = ?").await?;
        let get_concept = session.prepare("SELECT id, name, owner, type, processed FROM media_concepts WHERE id = ?").await?;

        Ok(ScyllaDb {
            session,
            get_unprocessed_concepts,
            mark_concept_processed,
            mark_concept_processed_by_owner,
            delete_concept,
            delete_concept_by_owner,
            delete_unprocessed_concept,
            get_concept,
        })
    }
}
