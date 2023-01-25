use std::vec;

use solvers_traits::tree::FmmData;

use super::morton::MortonKey;

pub enum NodeType {
    Default,
    Fmm,
}

#[derive(Debug, Clone)]
pub struct NodeData {
    field_size: Vec<usize>,
    data: Vec<f64>,
    displacement: Vec<usize>,
}

impl NodeData {
    pub fn new(node_type: NodeType) -> NodeData {
        match node_type {
            NodeType::Default => NodeData::default_data(),
            NodeType::Fmm => NodeData::fmm_data(),
        }
    }

    fn default_data() -> NodeData {
        // Stub
        NodeData {
            data: Vec::<f64>::new(),
            field_size: vec![1],
            displacement: vec![0],
        }
    }

    fn fmm_data() -> NodeData {
        NodeData {
            data: Vec::<f64>::new(),
            field_size: vec![1, 1],
            displacement: vec![0, 1],
        }
    }
}

impl FmmData for NodeData {
    type CoefficientDataType = Vec<f64>;
    type NodeIndex = MortonKey;

    fn set_expansion_order(&mut self, order: usize) {
        let ncoeffs = 6 * (order - 1).pow(2) + 2;
        self.field_size = vec![ncoeffs, ncoeffs];

        self.displacement = self
            .field_size
            .iter()
            .scan(0, |state, &x| {
                let tmp = *state;
                *state += x;
                Some(tmp)
            })
            .collect();
    }

    fn get_expansion_order(&self) -> usize {
        // stub
        if self.field_size[0] > 0 {
            (((self.field_size[0] - 2) / 6) as f64).sqrt() as usize + 1
        } else {
            0
        }
    }

    fn get_local_expansion(&self) -> Self::CoefficientDataType {
        self.data[self.displacement[0]..self.displacement[1]].to_vec()
    }

    fn get_multipole_expansion(&self) -> Self::CoefficientDataType {
        self.data[self.displacement[0]..self.displacement[1]].to_vec()
    }

    fn set_local_expansion(&mut self, data: &Self::CoefficientDataType) {
        let (_, mut _local) = data.split_at(self.displacement[1]);
        _local = data;
    }

    fn set_multipole_expansion(&mut self, data: &Self::CoefficientDataType) {
        let (mut _multipole, _) = data.split_at(self.displacement[1]);
        _multipole = data;
    }
}

#[cfg(test)]
mod test {
    use super::NodeData;
    use solvers_traits::tree::FmmData;

    #[test]
    fn test_fmm_node_data() {
        let mut data = NodeData::fmm_data();
        let order = 5;
        data.set_expansion_order(order);
    }
}