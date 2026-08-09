use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntityKind {
    Document,
    File,
    Folder,
}

impl EntityKind {
    pub fn route_name(self) -> &'static str {
        match self {
            Self::Document => "doc",
            Self::File => "file",
            Self::Folder => "folder",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectEntity {
    pub id: String,
    pub kind: EntityKind,
    pub name: String,
    pub path: String,
    pub parent_id: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProjectModelError {
    #[error("Overleaf project has no rootFolder[0]")]
    MissingRoot,
    #[error("Overleaf entity has no stable _id")]
    MissingId,
    #[error("Overleaf entity has no name")]
    MissingName,
    #[error("path escapes the local replica: {0}")]
    EscapingPath(String),
    #[error("duplicate Overleaf path: {0}")]
    DuplicatePath(String),
    #[error("unknown Overleaf parent folder: {0}")]
    UnknownParent(String),
}

#[derive(Clone, Debug)]
pub struct ProjectModel {
    project: Value,
    root_id: String,
    by_id: HashMap<String, ProjectEntity>,
    by_path: HashMap<String, String>,
}

impl ProjectModel {
    pub fn from_project(project: Value) -> Result<Self, ProjectModelError> {
        let root = project
            .get("rootFolder")
            .and_then(Value::as_array)
            .and_then(|folders| folders.first())
            .cloned()
            .ok_or(ProjectModelError::MissingRoot)?;
        let root_id = entity_id(&root)?;
        let mut model = Self {
            project,
            root_id,
            by_id: HashMap::new(),
            by_path: HashMap::new(),
        };
        model.index_folder(&root, "", None)?;
        Ok(model)
    }

    pub fn project(&self) -> &Value {
        &self.project
    }

    pub fn root_id(&self) -> &str {
        &self.root_id
    }

    pub fn all(&self) -> impl Iterator<Item = &ProjectEntity> {
        self.by_id.values()
    }

    pub fn documents(&self) -> impl Iterator<Item = &ProjectEntity> {
        self.all()
            .filter(|entity| entity.kind == EntityKind::Document)
    }

    pub fn files(&self) -> impl Iterator<Item = &ProjectEntity> {
        self.all().filter(|entity| entity.kind == EntityKind::File)
    }

    pub fn folders(&self) -> impl Iterator<Item = &ProjectEntity> {
        self.all()
            .filter(|entity| entity.kind == EntityKind::Folder)
    }

    pub fn get_by_id(&self, id: &str) -> Option<&ProjectEntity> {
        self.by_id.get(id)
    }

    pub fn get_by_path(&self, path: &str) -> Result<Option<&ProjectEntity>, ProjectModelError> {
        let path = normalize_relative_path(path)?;
        Ok(self
            .by_path
            .get(&path)
            .and_then(|entity_id| self.by_id.get(entity_id)))
    }

    pub fn parent_folder_for_path(
        &self,
        path: &str,
    ) -> Result<Option<&ProjectEntity>, ProjectModelError> {
        let path = normalize_relative_path(path)?;
        let parent_path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
        Ok(self
            .get_by_path(parent_path)?
            .filter(|entity| entity.kind == EntityKind::Folder))
    }

    pub fn insert(
        &mut self,
        parent_folder_id: &str,
        kind: EntityKind,
        raw: Value,
    ) -> Result<ProjectEntity, ProjectModelError> {
        let parent = self
            .by_id
            .get(parent_folder_id)
            .filter(|entity| entity.kind == EntityKind::Folder)
            .ok_or_else(|| ProjectModelError::UnknownParent(parent_folder_id.into()))?;
        let name = entity_name(&raw)?;
        let path = join_relative(&parent.path, &name)?;
        let entity = ProjectEntity {
            id: entity_id(&raw)?,
            kind,
            name,
            path,
            parent_id: Some(parent_folder_id.into()),
            raw,
        };
        self.register(entity.clone())?;
        if kind == EntityKind::Folder {
            self.index_folder_children(&entity.raw, &entity.path, &entity.id)?;
        }
        Ok(entity)
    }

    pub fn rename(
        &mut self,
        entity_id: &str,
        name: &str,
    ) -> Result<Option<(String, String)>, ProjectModelError> {
        let Some(entity) = self.by_id.get(entity_id).cloned() else {
            return Ok(None);
        };
        let parent_path = entity
            .parent_id
            .as_ref()
            .and_then(|parent| self.by_id.get(parent))
            .map(|parent| parent.path.as_str())
            .unwrap_or("");
        let next_path = if entity.parent_id.is_some() {
            join_relative(parent_path, name)?
        } else {
            String::new()
        };
        let old_path = entity.path;
        self.repath(entity_id, &next_path)?;
        if let Some(entity) = self.by_id.get_mut(entity_id) {
            entity.name = name.into();
            if let Some(raw) = entity.raw.as_object_mut() {
                raw.insert("name".into(), Value::String(name.into()));
            }
        }
        Ok(Some((old_path, next_path)))
    }

    pub fn move_to(
        &mut self,
        entity_id: &str,
        parent_folder_id: &str,
    ) -> Result<Option<(String, String)>, ProjectModelError> {
        let Some(entity) = self.by_id.get(entity_id).cloned() else {
            return Ok(None);
        };
        let parent = self
            .by_id
            .get(parent_folder_id)
            .filter(|entity| entity.kind == EntityKind::Folder)
            .ok_or_else(|| ProjectModelError::UnknownParent(parent_folder_id.into()))?;
        let next_path = join_relative(&parent.path, &entity.name)?;
        let old_path = entity.path;
        self.repath(entity_id, &next_path)?;
        if let Some(entity) = self.by_id.get_mut(entity_id) {
            entity.parent_id = Some(parent_folder_id.into());
        }
        Ok(Some((old_path, next_path)))
    }

    pub fn remove(&mut self, entity_id: &str) -> Vec<ProjectEntity> {
        let Some(entity) = self.by_id.get(entity_id).cloned() else {
            return Vec::new();
        };
        if entity.parent_id.is_none() {
            return Vec::new();
        }
        let prefix = format!("{}/", entity.path);
        let ids = self
            .by_id
            .values()
            .filter(|candidate| candidate.id == entity_id || candidate.path.starts_with(&prefix))
            .map(|candidate| candidate.id.clone())
            .collect::<Vec<_>>();
        ids.into_iter()
            .filter_map(|id| {
                let entity = self.by_id.remove(&id)?;
                self.by_path.remove(&entity.path);
                Some(entity)
            })
            .collect()
    }

    fn index_folder(
        &mut self,
        folder: &Value,
        path: &str,
        parent_id: Option<String>,
    ) -> Result<(), ProjectModelError> {
        let id = entity_id(folder)?;
        let entity = ProjectEntity {
            id: id.clone(),
            kind: EntityKind::Folder,
            name: folder
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            path: normalize_relative_path(path)?,
            parent_id,
            raw: folder.clone(),
        };
        self.register(entity)?;
        self.index_folder_children(folder, path, &id)
    }

    fn index_folder_children(
        &mut self,
        folder: &Value,
        folder_path: &str,
        folder_id: &str,
    ) -> Result<(), ProjectModelError> {
        for (kind, key) in [
            (EntityKind::Document, "docs"),
            (EntityKind::File, "fileRefs"),
            (EntityKind::Folder, "folders"),
        ] {
            for child in folder
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let child_path = join_relative(folder_path, &entity_name(child)?)?;
                if kind == EntityKind::Folder {
                    self.index_folder(child, &child_path, Some(folder_id.into()))?;
                } else {
                    self.register(ProjectEntity {
                        id: entity_id(child)?,
                        kind,
                        name: entity_name(child)?,
                        path: child_path,
                        parent_id: Some(folder_id.into()),
                        raw: child.clone(),
                    })?;
                }
            }
        }
        Ok(())
    }

    fn register(&mut self, entity: ProjectEntity) -> Result<(), ProjectModelError> {
        if self
            .by_path
            .get(&entity.path)
            .is_some_and(|existing| existing != &entity.id)
        {
            return Err(ProjectModelError::DuplicatePath(entity.path));
        }
        self.by_path.insert(entity.path.clone(), entity.id.clone());
        self.by_id.insert(entity.id.clone(), entity);
        Ok(())
    }

    fn repath(&mut self, entity_id: &str, next_path: &str) -> Result<(), ProjectModelError> {
        let Some(entity) = self.by_id.get(entity_id) else {
            return Ok(());
        };
        let old_path = entity.path.clone();
        let prefix = format!("{old_path}/");
        let changes = self
            .by_id
            .values()
            .filter(|candidate| candidate.id == entity_id || candidate.path.starts_with(&prefix))
            .map(|candidate| {
                let suffix = candidate.path.strip_prefix(&prefix).unwrap_or_default();
                let path = if candidate.id == entity_id {
                    next_path.to_owned()
                } else {
                    join_relative(next_path, suffix)?
                };
                Ok((candidate.id.clone(), candidate.path.clone(), path))
            })
            .collect::<Result<Vec<_>, ProjectModelError>>()?;
        for (_, old_path, _) in &changes {
            self.by_path.remove(old_path);
        }
        for (id, _, path) in changes {
            if self
                .by_path
                .get(&path)
                .is_some_and(|existing| existing != &id)
            {
                return Err(ProjectModelError::DuplicatePath(path));
            }
            self.by_path.insert(path.clone(), id.clone());
            if let Some(entity) = self.by_id.get_mut(&id) {
                entity.path = path;
            }
        }
        Ok(())
    }
}

pub fn normalize_relative_path(path: &str) -> Result<String, ProjectModelError> {
    let original = path.to_owned();
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/')
        || normalized
            .as_bytes()
            .get(1)
            .is_some_and(|character| *character == b':')
    {
        return Err(ProjectModelError::EscapingPath(original));
    }
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(ProjectModelError::EscapingPath(original)),
            part => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

fn join_relative(parent: &str, child: &str) -> Result<String, ProjectModelError> {
    if parent.is_empty() {
        normalize_relative_path(child)
    } else {
        normalize_relative_path(&format!("{parent}/{child}"))
    }
}

fn entity_id(value: &Value) -> Result<String, ProjectModelError> {
    value
        .get("_id")
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or(ProjectModelError::MissingId)
}

fn entity_name(value: &Value) -> Result<String, ProjectModelError> {
    value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .ok_or(ProjectModelError::MissingName)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn fixture() -> Value {
        json!({
            "name": "Paper",
            "rootFolder": [{
                "_id": "root",
                "name": "root",
                "docs": [{ "_id": "main", "name": "main.tex" }],
                "fileRefs": [{ "_id": "figure", "name": "figure.png" }],
                "folders": [{
                    "_id": "sections",
                    "name": "sections",
                    "docs": [{ "_id": "intro", "name": "intro.tex" }],
                    "fileRefs": [],
                    "folders": []
                }]
            }]
        })
    }

    #[test]
    fn indexes_and_mutates_a_project_tree_by_stable_id() {
        let mut model = ProjectModel::from_project(fixture()).unwrap();
        assert_eq!(model.root_id(), "root");
        assert_eq!(model.documents().count(), 2);
        assert_eq!(
            model.get_by_path("sections/intro.tex").unwrap().unwrap().id,
            "intro"
        );

        model.rename("sections", "chapters").unwrap();
        assert!(model.get_by_path("sections/intro.tex").unwrap().is_none());
        assert_eq!(model.get_by_id("intro").unwrap().path, "chapters/intro.tex");

        model.move_to("figure", "sections").unwrap();
        assert_eq!(
            model.get_by_id("figure").unwrap().path,
            "chapters/figure.png"
        );
        assert_eq!(model.remove("sections").len(), 3);
        assert!(model.get_by_id("intro").is_none());
    }

    #[test]
    fn rejects_paths_that_escape_the_replica() {
        assert_eq!(
            normalize_relative_path("sections/../../secret"),
            Err(ProjectModelError::EscapingPath(
                "sections/../../secret".into()
            ))
        );
        assert!(normalize_relative_path("/private/main.tex").is_err());
        assert!(normalize_relative_path("C:\\paper\\main.tex").is_err());
    }
}
