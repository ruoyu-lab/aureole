//! 动态子元素：按 key 增量更新的列表（`each`）和条件显示（`child_when`）。

use std::collections::HashMap;
use std::hash::Hash;
use std::rc::Rc;

use reactive::{Effect, Scope, untracked};

use crate::element::View;
use crate::tree::{self, NodeId, TreeRef};

/// 挂载后由一个副作用维护的一组子元素
pub(crate) trait Dynamic {
    /// 挂到 `parent` 的第 `slot` 个子元素位置上
    fn mount(self: Box<Self>, tree: &TreeRef, parent: NodeId, slot: usize);
}

pub(crate) struct Each<I, K, V> {
    pub items: I,
    pub key: K,
    pub view: V,
}

impl<T, Key, I, K, V, R> Dynamic for Each<I, K, V>
where
    T: 'static,
    Key: Eq + Hash + 'static,
    I: Fn() -> Vec<T> + 'static,
    K: Fn(&T) -> Key + 'static,
    V: Fn(T) -> R + 'static,
    R: View,
{
    fn mount(self: Box<Self>, tree: &TreeRef, parent: NodeId, slot: usize) {
        let Each { items, key, view } = *self;
        // 每一项的作用域都挂在这个作用域下，而不是挂在下面的副作用上：
        // 列表变化、副作用重新执行时，保留下来的项不能被一起释放
        let owner = Scope::new();
        let tree = Rc::downgrade(tree);
        let mut current: Vec<(Key, NodeId, Scope)> = Vec::new();

        Effect::new(move || {
            // 只订阅列表本身；下面构建视图时读到的信号不订阅
            let items = items();
            let Some(tree) = tree.upgrade() else { return };
            untracked(|| {
                let mut old: HashMap<Key, (NodeId, Scope)> = HashMap::with_capacity(current.len());
                let mut removed = Vec::new();
                for (k, node, scope) in current.drain(..) {
                    // 上一轮有重复 key 时，多出来的那项也要清掉
                    if let Some(duplicate) = old.insert(k, (node, scope)) {
                        removed.push(duplicate);
                    }
                }

                let mut next = Vec::with_capacity(items.len());
                for item in items {
                    let k = key(&item);
                    let (node, scope) = old.remove(&k).unwrap_or_else(|| {
                        let scope = owner.run(Scope::new);
                        let node = scope.run(|| tree::mount(&tree, view(item).into_element()));
                        (node, scope)
                    });
                    next.push((k, node, scope));
                }

                // 不再出现的项：释放作用域（绑定和项内的信号）；有退场动画的先退场，否则直接卸载
                removed.extend(old.into_values());
                for (node, scope) in removed {
                    scope.dispose();
                    let mut t = tree.borrow_mut();
                    if t.has_exit(node) {
                        t.begin_exit(node);
                    } else {
                        t.unmount(node);
                    }
                }
                tree.borrow_mut().set_dynamic(parent, slot, next.iter().map(|(_, node, _)| *node).collect());
                current = next;
            });
        });
    }
}
