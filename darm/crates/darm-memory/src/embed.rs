//! Text embeddings for associative recall.
//!
//! [`HashingEmbedder`] is a dependency-free feature-hashing embedder
//! (unigrams + bigrams, signed hashing, L2-normalized). It is deterministic,
//! needs no model, and is good enough for keyword-level association. A node
//! with an embedding model in the mesh can implement [`Embedder`] on top of
//! it; every node only compares vectors it computed itself, so mixing
//! embedders across nodes is safe.

pub trait Embedder: Send + Sync {
    fn dimensions(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
}

#[derive(Debug, Clone)]
pub struct HashingEmbedder {
    dimensions: usize,
}

impl HashingEmbedder {
    pub fn new(dimensions: usize) -> Self {
        Self {
            dimensions: dimensions.max(16),
        }
    }
}

impl Default for HashingEmbedder {
    fn default() -> Self {
        Self::new(512)
    }
}

/// Lowercased alphanumeric tokens, dropping very short and very common words.
pub fn tokens(text: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "are", "but", "not", "you", "all", "can", "had", "her", "was", "one",
        "our", "out", "has", "have", "this", "that", "with", "from", "they", "what", "your",
        "will", "would", "there", "their", "about", "which", "when", "into", "than", "then",
        "them", "these", "some", "could", "how", "does", "did", "its", "it's",
    ];
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(str::to_lowercase)
        .filter(|t| !STOP.contains(&t.as_str()))
        .collect()
}

impl Embedder for HashingEmbedder {
    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0f32; self.dimensions];
        let words = tokens(text);
        let mut add = |feature: &str, weight: f32| {
            let hash = blake3::hash(feature.as_bytes());
            let bytes = hash.as_bytes();
            let index = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes")) as usize
                % self.dimensions;
            let sign = if bytes[8] & 1 == 0 { 1.0 } else { -1.0 };
            vector[index] += sign * weight;
        };
        for word in &words {
            add(word, 1.0);
        }
        for pair in words.windows(2) {
            add(&format!("{} {}", pair[0], pair[1]), 0.5);
        }
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut vector {
                *value /= norm;
            }
        }
        vector
    }
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn related_text_scores_higher() {
        let e = HashingEmbedder::default();
        let q = e.embed("how do I rotate the TLS certificate on the gateway");
        let close = e.embed("Rotate the gateway TLS certificate with certbot renew");
        let far = e.embed("banana bread recipe with walnuts");
        assert!(cosine(&q, &close) > cosine(&q, &far) + 0.2);
    }

    #[test]
    fn empty_text_is_zero_vector() {
        let e = HashingEmbedder::new(32);
        assert!(e.embed("  ").iter().all(|v| *v == 0.0));
    }
}
