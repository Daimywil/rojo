//! Includes assets when building models and places.

use std::collections::HashMap;

use anyhow::Context;
use rayon::prelude::*;
use rbx_dom_weak::{types::Ref, InstanceBuilder, WeakDom};

use crate::snapshot::RojoTree;

use super::{names, store::AssetStore};

/// Returns the indices that lead from the root of a DOM to an instance.
fn index_path(dom: &WeakDom, referent: Ref) -> Vec<usize> {
    let mut path = Vec::new();
    let mut current = referent;

    while current != dom.root_ref() {
        let instance = dom.get_by_ref(current).unwrap();
        let parent = dom.get_by_ref(instance.parent()).unwrap();
        let index = parent
            .children()
            .iter()
            .position(|child| *child == current)
            .unwrap();
        path.push(index);
        current = instance.parent();
    }

    path.reverse();
    path
}

fn follow_index_path(dom: &WeakDom, path: &[usize]) -> Ref {
    path.iter().fold(dom.root_ref(), |current, index| {
        dom.get_by_ref(current).unwrap().children()[*index]
    })
}

/// Builds a copy of the tree's DOM with the contents of every asset directory
/// added to it. Returns `None` if the project has no asset directories, in
/// which case the tree's DOM can be used as-is.
pub fn dom_with_assets(tree: &RojoTree, store: &AssetStore) -> anyhow::Result<Option<WeakDom>> {
    let containers = store.containers();
    if containers.is_empty() {
        return Ok(None);
    }

    let source = tree.inner();
    let source_root = source.root();

    let mut dom = WeakDom::new(
        InstanceBuilder::new(source_root.class)
            .with_name(source_root.name.clone())
            .with_properties(source_root.properties.clone()),
    );

    let cloned = source.clone_multiple_into_external(source_root.children(), &mut dom);
    let root_ref = dom.root_ref();
    for child in cloned {
        dom.transfer_within(child, root_ref);
    }

    for container in containers {
        let target = follow_index_path(&dom, &index_path(source, container.id));

        let files = store.load_all(container.id)?;
        let parsed: Vec<(String, WeakDom)> = files
            .into_par_iter()
            .map(|(path, contents)| {
                let model = rbx_binary::from_reader(contents.as_slice())
                    .with_context(|| format!("Could not read asset {path} in {}", container.key))?;
                Ok((path, model))
            })
            .collect::<anyhow::Result<_>>()?;

        let mut folders: HashMap<String, Ref> = HashMap::new();

        for (path, mut model) in parsed {
            let mut parent = target;
            let mut folder_path = String::new();

            let components: Vec<&str> = path.split('/').collect();
            for component in &components[..components.len() - 1] {
                if !folder_path.is_empty() {
                    folder_path.push('/');
                }
                folder_path.push_str(component);

                parent = *folders.entry(folder_path.clone()).or_insert_with(|| {
                    // Directories that don't decode are never indexed, so this
                    // always has a name.
                    let name = names::folder_name_from_dir(component).unwrap_or_default();
                    dom.insert(parent, InstanceBuilder::new("Folder").with_name(name))
                });
            }

            let roots = model.root().children().to_vec();
            for root in roots {
                model.transfer(root, &mut dom, parent);
            }
        }
    }

    Ok(Some(dom))
}

#[cfg(test)]
mod test {
    use super::*;

    use std::fs;

    use crate::{
        assets::rbxm,
        snapshot::{InstanceMetadata, InstanceSnapshot},
    };

    fn model_file(class: &str, name: &str) -> Vec<u8> {
        let mut dom = WeakDom::new(InstanceBuilder::new("DataModel"));
        let instance = dom.insert(dom.root_ref(), InstanceBuilder::new(class).with_name(name));
        dom.insert(instance, InstanceBuilder::new("Part").with_name("Child"));

        let mut output = Vec::new();
        rbx_binary::to_writer(&mut output, &dom, &[instance]).unwrap();
        rbxm::canonicalize(&output).unwrap()
    }

    #[test]
    fn assets_are_added_under_their_container() {
        let dir = tempfile::tempdir().unwrap();
        let assets = dir.path().join("assets");
        fs::create_dir_all(assets.join("Maps - Bounty").join("Namek")).unwrap();
        fs::write(
            assets.join("Maps - Bounty").join("Namek").join("Rock.rbxm"),
            model_file("Model", "Rock"),
        )
        .unwrap();
        fs::write(assets.join("Part~2.rbxm"), model_file("Part", "Part")).unwrap();
        fs::write(assets.join("Part.rbxm"), model_file("Part", "Part")).unwrap();

        let mut tree = RojoTree::new(InstanceSnapshot::new().class_name("DataModel"));
        let storage = tree.insert_instance(
            tree.get_root_id(),
            InstanceSnapshot::new()
                .class_name("ServerStorage")
                .name("ServerStorage"),
        );
        tree.insert_instance(
            storage,
            InstanceSnapshot::new()
                .class_name("Folder")
                .name("Assets")
                .metadata(InstanceMetadata::new().asset_dir(Some(assets.clone()))),
        );

        let store = AssetStore::new(dir.path(), None);
        store.refresh_containers(&tree);

        let dom = dom_with_assets(&tree, &store).unwrap().unwrap();

        let storage = dom.get_by_ref(dom.root().children()[0]).unwrap();
        assert_eq!(storage.name, "ServerStorage");
        let container = dom.get_by_ref(storage.children()[0]).unwrap();
        assert_eq!(container.name, "Assets");

        let children: Vec<(String, String)> = container
            .children()
            .iter()
            .map(|child| {
                let child = dom.get_by_ref(*child).unwrap();
                (child.name.clone(), child.class.to_string())
            })
            .collect();
        assert_eq!(
            children,
            [
                ("Maps - Bounty".to_owned(), "Folder".to_owned()),
                ("Part".to_owned(), "Part".to_owned()),
                ("Part".to_owned(), "Part".to_owned()),
            ]
        );

        let maps = dom.get_by_ref(container.children()[0]).unwrap();
        let namek = dom.get_by_ref(maps.children()[0]).unwrap();
        assert_eq!(namek.name, "Namek");
        let rock = dom.get_by_ref(namek.children()[0]).unwrap();
        assert_eq!(rock.name, "Rock");
        assert_eq!(rock.class, "Model");
        assert_eq!(rock.children().len(), 1);
    }

    #[test]
    fn siblings_with_the_same_name_are_built_in_suffix_order() {
        let dir = tempfile::tempdir().unwrap();
        let assets = dir.path().join("assets");
        fs::create_dir_all(&assets).unwrap();

        for number in 1..=11 {
            let file_name = if number == 1 {
                "Part.rbxm".to_owned()
            } else {
                format!("Part~{number}.rbxm")
            };
            fs::write(
                assets.join(file_name),
                model_file("Part", &format!("{number}")),
            )
            .unwrap();
        }

        let mut tree = RojoTree::new(InstanceSnapshot::new().class_name("DataModel"));
        tree.insert_instance(
            tree.get_root_id(),
            InstanceSnapshot::new()
                .class_name("Folder")
                .name("Assets")
                .metadata(InstanceMetadata::new().asset_dir(Some(assets.clone()))),
        );

        let store = AssetStore::new(dir.path(), None);
        store.refresh_containers(&tree);
        let dom = dom_with_assets(&tree, &store).unwrap().unwrap();

        let container = dom.get_by_ref(dom.root().children()[0]).unwrap();
        let order: Vec<String> = container
            .children()
            .iter()
            .map(|child| dom.get_by_ref(*child).unwrap().name.clone())
            .collect();

        let expected: Vec<String> = (1..=11).map(|number| number.to_string()).collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn projects_without_assets_use_the_tree_directly() {
        let dir = tempfile::tempdir().unwrap();
        let tree = RojoTree::new(InstanceSnapshot::new().class_name("DataModel"));
        let store = AssetStore::new(dir.path(), None);
        store.refresh_containers(&tree);

        assert!(dom_with_assets(&tree, &store).unwrap().is_none());
    }
}
