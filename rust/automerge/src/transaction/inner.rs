use std::num::NonZeroU64;

use crate::automerge::Actor;
use crate::exid::ExId;
use crate::query::{self, OpIdSearch};
use crate::storage::Change as StoredChange;
use crate::types::{Key, ObjId, OpId, OpIds};
use crate::{op_tree::OpSetMetadata, types::Op, Automerge, Change, ChangeHash, OpObserver, Prop};
use crate::{AutomergeError, ObjType, OpType, ScalarValue};

#[derive(Debug, Clone)]
pub(crate) struct TransactionInner {
    actor: usize,
    seq: u64,
    start_op: NonZeroU64,
    time: i64,
    message: Option<String>,
    deps: Vec<ChangeHash>,
    operations: Vec<(ObjId, Op)>,
}

/// Arguments required to create a new transaction
pub(crate) struct TransactionArgs {
    /// The index of the actor ID this transaction will create ops for in the
    /// [`OpSetMetadata::actors`]
    pub(crate) actor_index: usize,
    /// The sequence number of the change this transaction will create
    pub(crate) seq: u64,
    /// The start op of the change this transaction will create
    pub(crate) start_op: NonZeroU64,
    /// The dependencies of the change this transaction will create
    pub(crate) deps: Vec<ChangeHash>,
}

impl TransactionInner {
    pub(crate) fn new(
        TransactionArgs {
            actor_index: actor,
            seq,
            start_op,
            deps,
        }: TransactionArgs,
    ) -> Self {
        TransactionInner {
            actor,
            seq,
            start_op,
            time: 0,
            message: None,
            operations: vec![],
            deps,
        }
    }

    /// Create an empty change
    pub(crate) fn empty(
        doc: &mut Automerge,
        args: TransactionArgs,
        message: Option<String>,
        time: Option<i64>,
    ) -> ChangeHash {
        Self::new(args).commit_impl(doc, message, time)
    }

    pub(crate) fn pending_ops(&self) -> usize {
        self.operations.len()
    }

    /// Commit the operations performed in this transaction, returning the hashes corresponding to
    /// the new heads.
    ///
    /// Returns `None` if there were no operations to commit
    #[tracing::instrument(skip(self, doc))]
    pub(crate) fn commit(
        self,
        doc: &mut Automerge,
        message: Option<String>,
        time: Option<i64>,
    ) -> Option<ChangeHash> {
        if self.pending_ops() == 0 {
            return None;
        }
        Some(self.commit_impl(doc, message, time))
    }

    pub(crate) fn commit_impl(
        mut self,
        doc: &mut Automerge,
        message: Option<String>,
        time: Option<i64>,
    ) -> ChangeHash {
        if message.is_some() {
            self.message = message;
        }

        if let Some(t) = time {
            self.time = t;
        }

        let num_ops = self.pending_ops();
        let change = self.export(&doc.ops.m);
        let hash = change.hash();
        #[cfg(not(debug_assertions))]
        tracing::trace!(commit=?hash, deps=?change.deps(), "committing transaction");
        #[cfg(debug_assertions)]
        {
            let ops = change.iter_ops().collect::<Vec<_>>();
            tracing::trace!(commit=?hash, ?ops, deps=?change.deps(), "committing transaction");
        }
        doc.update_history(change, num_ops);
        debug_assert_eq!(doc.get_heads(), vec![hash]);
        hash
    }

    #[tracing::instrument(skip(self, metadata))]
    pub(crate) fn export(self, metadata: &OpSetMetadata) -> Change {
        use crate::storage::{change::PredOutOfOrder, convert::op_as_actor_id};

        let actor = metadata.actors.get(self.actor).clone();
        let deps = self.deps.clone();
        let stored = match StoredChange::builder()
            .with_actor(actor)
            .with_seq(self.seq)
            .with_start_op(self.start_op)
            .with_message(self.message.clone())
            .with_dependencies(deps)
            .with_timestamp(self.time)
            .build(
                self.operations
                    .iter()
                    .map(|(obj, op)| op_as_actor_id(obj, op, metadata)),
            ) {
            Ok(s) => s,
            Err(PredOutOfOrder) => {
                // SAFETY: types::Op::preds is `types::OpIds` which ensures ops are always sorted
                panic!("preds out of order");
            }
        };
        #[cfg(debug_assertions)]
        {
            let realized_ops = self.operations.iter().collect::<Vec<_>>();
            tracing::trace!(?stored, ops=?realized_ops, "committing change");
        }
        #[cfg(not(debug_assertions))]
        tracing::trace!(?stored, "committing change");
        Change::new(stored)
    }

    /// Undo the operations added in this transaction, returning the number of cancelled
    /// operations.
    pub(crate) fn rollback(self, doc: &mut Automerge) -> usize {
        let num = self.pending_ops();
        // remove in reverse order so sets are removed before makes etc...
        for (obj, op) in self.operations.into_iter().rev() {
            for pred_id in &op.pred {
                if let Some(p) = doc.ops.search(&obj, OpIdSearch::new(*pred_id)).index() {
                    doc.ops.change_vis(&obj, p, |o| o.remove_succ(&op));
                }
            }
            if let Some(pos) = doc.ops.search(&obj, OpIdSearch::new(op.id)).index() {
                doc.ops.remove(&obj, pos);
            }
        }

        // remove the actor from the cache so that it doesn't end up in the saved document
        if doc.states.get(&self.actor).is_none() && doc.ops.m.actors.len() > 0 {
            let actor = doc.ops.m.actors.remove_last();
            doc.actor = Actor::Unused(actor);
        }

        num
    }

    /// Set the value of property `P` to value `V` in object `obj`.
    ///
    /// # Returns
    ///
    /// The opid of the operation which was created, or None if this operation doesn't change the
    /// document
    ///
    /// # Errors
    ///
    /// This will return an error if
    /// - The object does not exist
    /// - The key is the wrong type for the object
    /// - The key does not exist in the object
    pub(crate) fn put<P: Into<Prop>, V: Into<ScalarValue>, Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        ex_obj: &ExId,
        prop: P,
        value: V,
    ) -> Result<(), AutomergeError> {
        let obj = doc.exid_to_obj(ex_obj)?;
        let value = value.into();
        let prop = prop.into();
        let obj_type = doc
            .ops
            .object_type(&obj)
            .ok_or(AutomergeError::NotAnObject)?;
        match (&prop, obj_type) {
            (Prop::Map(_), ObjType::Map) => Ok(()),
            (Prop::Seq(_), ObjType::List) => Ok(()),
            _ => Err(AutomergeError::InvalidOp(obj_type)),
        }?;
        self.local_op(doc, op_observer, obj, prop, value.into())?;
        Ok(())
    }

    /// Set the value of property `P` to value `V` in object `obj`.
    ///
    /// # Returns
    ///
    /// The opid of the operation which was created, or None if this operation doesn't change the
    /// document
    ///
    /// # Errors
    ///
    /// This will return an error if
    /// - The object does not exist
    /// - The key is the wrong type for the object
    /// - The key does not exist in the object
    pub(crate) fn put_object<P: Into<Prop>, Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        ex_obj: &ExId,
        prop: P,
        value: ObjType,
    ) -> Result<ExId, AutomergeError> {
        let obj = doc.exid_to_obj(ex_obj)?;
        let prop = prop.into();
        let obj_type = doc
            .ops
            .object_type(&obj)
            .ok_or(AutomergeError::NotAnObject)?;
        match (&prop, obj_type) {
            (Prop::Map(_), ObjType::Map) => Ok(()),
            (Prop::Seq(_), ObjType::List) => Ok(()),
            _ => Err(AutomergeError::InvalidOp(obj_type)),
        }?;
        let id = self
            .local_op(doc, op_observer, obj, prop, value.into())?
            .unwrap();
        let id = doc.id_to_exid(id);
        Ok(id)
    }

    fn next_id(&mut self) -> OpId {
        OpId(self.start_op.get() + self.pending_ops() as u64, self.actor)
    }

    fn next_insert(&mut self, key: Key, value: ScalarValue) -> Op {
        Op {
            id: self.next_id(),
            action: OpType::Put(value),
            key,
            succ: Default::default(),
            pred: Default::default(),
            insert: true,
        }
    }

    fn next_delete(&mut self, key: Key, pred: OpIds) -> Op {
        Op {
            id: self.next_id(),
            action: OpType::Delete,
            key,
            succ: Default::default(),
            pred,
            insert: false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_local_op<Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        prop: Prop,
        op: Op,
        pos: usize,
        obj: ObjId,
        succ_pos: &[usize],
    ) {
        doc.ops.add_succ(&obj, succ_pos, &op);

        if !op.is_delete() {
            doc.ops.insert(pos, &obj, op.clone());
        }

        self.finalize_op(doc, op_observer, obj, prop, op);
    }

    pub(crate) fn insert<V: Into<ScalarValue>, Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        ex_obj: &ExId,
        index: usize,
        value: V,
    ) -> Result<(), AutomergeError> {
        let obj = doc.exid_to_obj(ex_obj)?;
        let obj_type = doc
            .ops
            .object_type(&obj)
            .ok_or(AutomergeError::NotAnObject)?;
        if obj_type != ObjType::List {
            return Err(AutomergeError::InvalidOp(obj_type));
        }
        let value = value.into();
        tracing::trace!(obj=?obj, value=?value, "inserting value");
        self.do_insert(doc, op_observer, obj, index, value.into())?;
        Ok(())
    }

    pub(crate) fn insert_object<Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        ex_obj: &ExId,
        index: usize,
        value: ObjType,
    ) -> Result<ExId, AutomergeError> {
        let obj = doc.exid_to_obj(ex_obj)?;
        let obj_type = doc
            .ops
            .object_type(&obj)
            .ok_or(AutomergeError::NotAnObject)?;
        if obj_type != ObjType::List {
            return Err(AutomergeError::InvalidOp(obj_type));
        }
        let id = self.do_insert(doc, op_observer, obj, index, value.into())?;
        let id = doc.id_to_exid(id);
        Ok(id)
    }

    fn do_insert<Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        obj: ObjId,
        index: usize,
        action: OpType,
    ) -> Result<OpId, AutomergeError> {
        let id = self.next_id();

        let query = doc.ops.search(&obj, query::InsertNth::new(index, false));

        let key = query.key()?;

        let op = Op {
            id,
            action,
            key,
            succ: Default::default(),
            pred: Default::default(),
            insert: true,
        };

        doc.ops.insert(query.pos(), &obj, op.clone());

        self.finalize_op(doc, op_observer, obj, Prop::Seq(index), op);

        Ok(id)
    }

    pub(crate) fn local_op<Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        obj: ObjId,
        prop: Prop,
        action: OpType,
    ) -> Result<Option<OpId>, AutomergeError> {
        match prop {
            Prop::Map(s) => self.local_map_op(doc, op_observer, obj, s, action),
            Prop::Seq(n) => self.local_list_op(doc, op_observer, obj, n, action),
        }
    }

    fn local_map_op<Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        obj: ObjId,
        prop: String,
        action: OpType,
    ) -> Result<Option<OpId>, AutomergeError> {
        if prop.is_empty() {
            return Err(AutomergeError::EmptyStringKey);
        }

        let id = self.next_id();
        let prop_index = doc.ops.m.props.cache(prop.clone());
        let query = doc.ops.search(&obj, query::Prop::new(prop_index));

        // no key present to delete
        if query.ops.is_empty() && action == OpType::Delete {
            return Ok(None);
        }

        if query.ops.len() == 1 && query.ops[0].is_noop(&action) {
            return Ok(None);
        }

        // increment operations are only valid against counter values.
        // if there are multiple values (from conflicts) then we just need one of them to be a counter.
        if matches!(action, OpType::Increment(_)) && query.ops.iter().all(|op| !op.is_counter()) {
            return Err(AutomergeError::MissingCounter);
        }

        let pred = doc.ops.m.sorted_opids(query.ops.iter().map(|o| o.id));

        let op = Op {
            id,
            action,
            key: Key::Map(prop_index),
            succ: Default::default(),
            pred,
            insert: false,
        };

        let pos = query.pos;
        let ops_pos = query.ops_pos;
        self.insert_local_op(doc, op_observer, Prop::Map(prop), op, pos, obj, &ops_pos);

        Ok(Some(id))
    }

    fn local_list_op<Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        obj: ObjId,
        index: usize,
        action: OpType,
    ) -> Result<Option<OpId>, AutomergeError> {
        let query = doc.ops.search(&obj, query::Nth::new(index, false));

        let id = self.next_id();
        let pred = doc.ops.m.sorted_opids(query.ops.iter().map(|o| o.id));
        let key = query.key()?;

        if query.ops.len() == 1 && query.ops[0].is_noop(&action) {
            return Ok(None);
        }

        // increment operations are only valid against counter values.
        // if there are multiple values (from conflicts) then we just need one of them to be a counter.
        if matches!(action, OpType::Increment(_)) && query.ops.iter().all(|op| !op.is_counter()) {
            return Err(AutomergeError::MissingCounter);
        }

        let op = Op {
            id,
            action,
            key,
            succ: Default::default(),
            pred,
            insert: false,
        };

        let pos = query.pos;
        let ops_pos = query.ops_pos;
        self.insert_local_op(doc, op_observer, Prop::Seq(index), op, pos, obj, &ops_pos);

        Ok(Some(id))
    }

    pub(crate) fn increment<P: Into<Prop>, Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        obj: &ExId,
        prop: P,
        value: i64,
    ) -> Result<(), AutomergeError> {
        let obj = doc.exid_to_obj(obj)?;
        self.local_op(doc, op_observer, obj, prop.into(), OpType::Increment(value))?;
        Ok(())
    }

    pub(crate) fn delete<P: Into<Prop>, Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        ex_obj: &ExId,
        prop: P,
    ) -> Result<(), AutomergeError> {
        let obj = doc.exid_to_obj(ex_obj)?;
        let prop = prop.into();
        let obj_type = doc
            .ops
            .object_type(&obj)
            .ok_or(AutomergeError::NotAnObject)?;
        if obj_type == ObjType::Text {
            let pos = prop.to_index().ok_or(AutomergeError::InvalidOp(obj_type))?;
            self.inner_splice(
                doc,
                op_observer,
                obj,
                pos,
                1,
                vec![],
                SpliceType::Text("", doc.utf16),
            )?;
        } else {
            self.local_op(doc, op_observer, obj, prop, OpType::Delete)?;
        }
        Ok(())
    }

    /// Splice new elements into the given sequence. Returns a vector of the OpIds used to insert
    /// the new elements
    pub(crate) fn splice<Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        ex_obj: &ExId,
        pos: usize,
        del: usize,
        vals: impl IntoIterator<Item = ScalarValue>,
    ) -> Result<(), AutomergeError> {
        let obj = doc.exid_to_obj(ex_obj)?;
        let obj_type = doc
            .ops
            .object_type(&obj)
            .ok_or(AutomergeError::NotAnObject)?;
        if obj_type != ObjType::List {
            return Err(AutomergeError::InvalidOp(obj_type));
        }
        let vals = vals.into_iter().collect();
        self.inner_splice(doc, op_observer, obj, pos, del, vals, SpliceType::List)
    }

    /// Splice string into a text object
    pub(crate) fn splice_text<Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        ex_obj: &ExId,
        pos: usize,
        del: usize,
        text: &str,
    ) -> Result<(), AutomergeError> {
        let obj = doc.exid_to_obj(ex_obj)?;
        let obj_type = doc
            .ops
            .object_type(&obj)
            .ok_or(AutomergeError::NotAnObject)?;
        if obj_type != ObjType::Text {
            return Err(AutomergeError::InvalidOp(obj_type));
        }
        let vals = text.chars().map(ScalarValue::from).collect();
        self.inner_splice(
            doc,
            op_observer,
            obj,
            pos,
            del,
            vals,
            SpliceType::Text(text, doc.utf16),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn inner_splice<Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        mut op_observer: Option<&mut Obs>,
        obj: ObjId,
        mut index: usize,
        mut del: usize,
        vals: Vec<ScalarValue>,
        splice_type: SpliceType<'_>,
    ) -> Result<(), AutomergeError> {
        let ex_obj = doc.ops.id_to_exid(obj.0);
        let utf16 = splice_type.is_utf16();
        let mut index8 = index;
        let mut index16 = index;
        let mut del16 = 0;

        // delete `del` items - performing the query for each one
        let mut i = 0;
        while i < del {
            // TODO: could do this with a single custom query
            let query = doc.ops.search(&obj, query::Nth::new(index, utf16));

            // adjust index if we happen to be on a half character
            index = query.index();

            if i == 0 {
                // set here incase no inserts
                index8 = query.index_utf8();
                index16 = query.index_utf16();
            }

            let step = if let Some(op) = query.ops.last() {
                op.width(utf16)
            } else {
                // truncate the delete if past the end
                del = i;
                break;
            };

            let op = self.next_delete(query.key()?, query.pred(&doc.ops));

            doc.ops.add_succ(&obj, &query.ops_pos, &op);

            self.operations.push((obj, op));

            del16 += step;

            i += step;
        }

        if del > 0 {
            if let Some(obs) = op_observer.as_mut() {
                if utf16 {
                    obs.delete_utf16(doc, ex_obj.clone(), (index8, index16), (del, del16));
                } else {
                    // TODO: obs.delete() should take a length
                    for _ in 0..del {
                        obs.delete(doc, ex_obj.clone(), Prop::Seq(index));
                    }
                }
            }
        }

        // do the insert query for the first item and then
        // insert the remaining ops one after the other
        if !vals.is_empty() {
            let query = doc.ops.search(&obj, query::InsertNth::new(index, utf16));
            let mut pos = query.pos();
            let mut key = query.key()?;

            // set here incase no deletes
            index8 = query.index_utf8();
            index16 = query.index_utf16();
            let len8 = vals.len();

            for v in &vals {
                let op = self.next_insert(key, v.clone());

                doc.ops.insert(pos, &obj, op.clone());

                pos += 1;
                key = op.id.into();

                self.operations.push((obj, op));
            }

            // handle the observer
            if let Some(obs) = op_observer.as_mut() {
                match splice_type {
                    SpliceType::List => {
                        let start = self.operations.len() - len8;
                        for (offset, v) in vals.iter().enumerate() {
                            let op = &self.operations[start + offset].1;
                            let value = (v.clone().into(), doc.ops.id_to_exid(op.id));
                            obs.insert(doc, ex_obj.clone(), index + offset, value)
                        }
                    }
                    SpliceType::Text(text, utf16) => {
                        if utf16 {
                            let len16 = text.encode_utf16().count();
                            obs.splice_text_utf16(
                                doc,
                                ex_obj,
                                (index8, index16),
                                (len8, len16),
                                text,
                            )
                        } else {
                            obs.splice_text(doc, ex_obj, index8, text)
                        }
                    }
                }
            }
        }

        Ok(())
    }

    fn finalize_op<Obs: OpObserver>(
        &mut self,
        doc: &mut Automerge,
        op_observer: Option<&mut Obs>,
        obj: ObjId,
        prop: Prop,
        op: Op,
    ) {
        // TODO - id_to_exid should be a noop if not used - change type to Into<ExId>?
        if let Some(op_observer) = op_observer {
            let ex_obj = doc.ops.id_to_exid(obj.0);
            if op.insert {
                let obj_type = doc.ops.object_type(&obj);
                match (obj_type, prop) {
                    (Some(ObjType::List), Prop::Seq(index)) => {
                        let value = (op.value(), doc.ops.id_to_exid(op.id));
                        op_observer.insert(doc, ex_obj, index, value)
                    }
                    (Some(ObjType::Text), Prop::Seq(index)) => {
                        // FIXME
                        op_observer.splice_text(doc, ex_obj, index, op.to_str())
                    }
                    // this should be a warning - not a panic
                    _ => panic!("insert into a map"),
                }
            } else if op.is_delete() {
                op_observer.delete(doc, ex_obj, prop);
            } else if let Some(value) = op.get_increment_value() {
                op_observer.increment(doc, ex_obj, prop, (value, doc.ops.id_to_exid(op.id)));
            } else {
                let value = (op.value(), doc.ops.id_to_exid(op.id));
                op_observer.put(doc, ex_obj, prop, value, false);
            }
        }
        self.operations.push((obj, op));
    }
}

enum SpliceType<'a> {
    List,
    Text(&'a str, bool /* utf16 */),
}

impl<'a> SpliceType<'a> {
    fn is_utf16(&self) -> bool {
        matches!(self, SpliceType::Text(_, true))
    }
}

#[cfg(test)]
mod tests {
    use crate::{transaction::Transactable, ROOT};

    use super::*;

    #[test]
    fn map_rollback_doesnt_panic() {
        let mut doc = Automerge::new();
        let mut tx = doc.transaction();

        let a = tx.put_object(ROOT, "a", ObjType::Map).unwrap();
        tx.put(&a, "b", 1).unwrap();
        assert!(tx.get(&a, "b").unwrap().is_some());
    }
}