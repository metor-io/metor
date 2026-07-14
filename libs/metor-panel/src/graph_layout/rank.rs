//! Layer assignment: which column (left→right) each node occupies.

use super::LayoutEdge;

/// Longest-path layering over the ranked skeleton. Relaxation is capped at
/// `n` passes so an unexpected residual cycle terminates deterministically
/// instead of looping (the caller's declared feedback edges are already
/// excluded from the skeleton).
pub(super) fn assign_layers(n: usize, edges: &[LayoutEdge]) -> Vec<usize> {
    let mut skeleton: Vec<(usize, usize)> = edges
        .iter()
        .filter(|e| e.ranked && !e.back)
        .map(|e| (e.from, e.to))
        .collect();
    skeleton.sort_unstable();

    let mut layer = vec![0usize; n];
    for _ in 0..n.max(1) {
        let mut changed = false;
        for &(u, v) in &skeleton {
            if layer[v] < layer[u] + 1 {
                layer[v] = layer[u] + 1;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    tighten_sources(&mut layer, &skeleton);
    layer
}

/// Longest-path pins every source at layer 0, which manufactures long edges
/// for sources whose consumers sit deep in the graph. Pull each source
/// forward to just before its earliest successor instead; only sources move,
/// so no successor constraint can break.
fn tighten_sources(layer: &mut [usize], skeleton: &[(usize, usize)]) {
    let mut has_pred = vec![false; layer.len()];
    let mut min_succ = vec![usize::MAX; layer.len()];
    for &(u, v) in skeleton {
        has_pred[v] = true;
        min_succ[u] = min_succ[u].min(layer[v]);
    }
    for u in 0..layer.len() {
        if !has_pred[u] && min_succ[u] != usize::MAX {
            let target = min_succ[u].saturating_sub(1);
            if target > layer[u] {
                layer[u] = target;
            }
        }
    }
}
