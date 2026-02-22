use anyhow::Result;

use crate::DomainError;

use super::super::Container;

pub struct DeleteController<'a> {
    container: &'a Container,
}

impl<'a> DeleteController<'a> {
    pub fn new(container: &'a Container) -> Self {
        Self { container }
    }

    pub async fn delete(&self, id_or_path: String) -> Result<String> {
        let use_case = self.container.delete_use_case();

        match use_case.execute(&id_or_path).await {
            Ok(_) => Ok(self.format_delete_success()),
            Err(e) => {
                // Only try path-based deletion if the ID was not found
                if matches!(e, DomainError::NotFound(_)) {
                    use_case.delete_by_path(&id_or_path).await?;
                    Ok(self.format_delete_success())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    fn format_delete_success(&self) -> String {
        "Repository deleted successfully.".to_string()
    }
}
