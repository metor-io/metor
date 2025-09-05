use arrow::{
    array::{
        Array, ArrayRef, ArrowPrimitiveType, BooleanArray, FixedSizeListArray, PrimitiveArray,
        RecordBatch, TimestampMicrosecondArray,
    },
    buffer::{BooleanBuffer, Buffer, ScalarBuffer},
    datatypes::*,
};
use convert_case::Casing;
use datafusion::{datasource::MemTable, prelude::SessionContext};
use futures_lite::{Stream, pin};
use impeller2::types::{PrimType, Timestamp};
use impeller2_wkt::ArchiveFormat;
use std::{
    fs::File,
    ops::{Bound, RangeBounds},
    path::Path,
    pin::Pin,
    ptr::NonNull,
    sync::Arc,
};
use zerocopy::{Immutable, IntoBytes};

use crate::{Component, DB, Error, append_log::AppendLog, time_series_2::TimeSeriesNode};

mod fft;
use fft::{FftUDF, FrequencyDomainUDF};

impl<T: IntoBytes + Immutable> AppendLog<T> {
    pub fn as_arrow_buffer(&self, element_size: usize) -> Buffer {
        self.as_arrow_buffer_range(.., element_size)
    }

    pub fn as_arrow_buffer_range<R: RangeBounds<usize>>(
        &self,
        range: R,
        element_size: usize,
    ) -> Buffer {
        let data = self.data();
        let start = match range.start_bound() {
            Bound::Included(&n) => n * element_size,
            Bound::Excluded(&n) => (n + 1) * element_size,
            Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            Bound::Included(&n) => (n + 1) * element_size,
            Bound::Excluded(&n) => n * element_size,
            Bound::Unbounded => data.len(),
        };
        let start = start.min(data.len());
        let end = end.min(data.len());
        let len = end.saturating_sub(start);

        unsafe {
            let ptr = NonNull::new(data.as_ptr().add(start) as *mut _).expect("mmap null");
            Buffer::from_custom_allocation(ptr, len, self.raw_mmap().clone())
        }
    }
}

impl TimeSeriesNode {
    pub fn as_data_array(
        &self,
        name: impl ToString,
        schema: &crate::ComponentSchema,
    ) -> (FieldRef, ArrayRef) {
        self.as_data_array_range(name, .., schema)
    }

    pub fn as_data_array_range<R: RangeBounds<usize>>(
        &self,
        name: impl ToString,
        range: R,
        schema: &crate::ComponentSchema,
    ) -> (FieldRef, ArrayRef) {
        let size = schema.dim.iter().product::<usize>() as i32;
        let element_size = self.element_size();
        let array = match schema.prim_type {
            PrimType::F64 => node_array_ref::<Float64Type>(self, range, element_size),
            PrimType::F32 => node_array_ref::<Float32Type>(self, range, element_size),
            PrimType::U64 => node_array_ref::<UInt64Type>(self, range, element_size),
            PrimType::U32 => node_array_ref::<UInt32Type>(self, range, element_size),
            PrimType::U16 => node_array_ref::<UInt16Type>(self, range, element_size),
            PrimType::U8 => node_array_ref::<UInt8Type>(self, range, element_size),
            PrimType::I64 => node_array_ref::<Int64Type>(self, range, element_size),
            PrimType::I32 => node_array_ref::<Int32Type>(self, range, element_size),
            PrimType::I16 => node_array_ref::<Int16Type>(self, range, element_size),
            PrimType::I8 => node_array_ref::<Int8Type>(self, range, element_size),
            PrimType::Bool => node_bool_ref(self, range, element_size),
        };

        let inner_field = Arc::new(Field::new(
            name.to_string(),
            array.data_type().clone(),
            false,
        ));

        if schema.dim.is_empty() {
            return (inner_field, array);
        }

        let field = Arc::new(Field::new_fixed_size_list(
            name.to_string(),
            inner_field.clone(),
            size,
            false,
        ));

        let array = FixedSizeListArray::new(inner_field, size, array, None);
        let array = Arc::new(array);
        (field, array)
    }

    pub fn as_time_series_array(&self) -> ArrayRef {
        self.as_time_series_array_range(..)
    }

    pub fn as_time_series_array_range<R: RangeBounds<usize>>(&self, range: R) -> ArrayRef {
        let buffer = self
            .index
            .as_arrow_buffer_range(range, size_of::<Timestamp>());
        let len = buffer.len() / std::mem::size_of::<i64>();
        let scalar_buffer = ScalarBuffer::<i64>::new(buffer, 0, len);
        let array = TimestampMicrosecondArray::new(scalar_buffer, None);
        Arc::new(array)
    }

    pub fn as_record_batch(
        &self,
        name: impl ToString,
        schema: &crate::ComponentSchema,
    ) -> RecordBatch {
        self.as_record_batch_range(name, .., schema)
    }

    pub fn as_record_batch_range(
        &self,
        name: impl ToString,
        range: impl RangeBounds<usize> + Clone,
        schema: &crate::ComponentSchema,
    ) -> RecordBatch {
        let name = name.to_string();
        let (data_field, data_array) =
            self.as_data_array_range(name.clone(), range.clone(), schema);
        let time_array = self.as_time_series_array_range(range);
        let len = data_array.len().min(time_array.len());
        let time_field = Arc::new(Field::new(
            "time",
            DataType::Timestamp(TimeUnit::Microsecond, None),
            false,
        ));
        let fields = vec![time_field, data_field];
        let columns = vec![time_array.slice(0, len), data_array.slice(0, len)];

        RecordBatch::try_new(Arc::new(Schema::new(fields)), columns)
            .expect("record batch params wrong")
    }
}

impl Component {
    pub fn as_mem_table(&self, name: impl ToString) -> Option<MemTable> {
        let name = name.to_string();
        let mut schema = None;

        let record_batches: Vec<_> = self
            .time_series
            .list
            .iter()
            .map(|node| {
                let record_batch = node.as_record_batch(&name, &self.schema);
                if schema.is_none() {
                    schema = Some(record_batch.schema());
                }
                record_batch
            })
            .collect();

        Some(
            MemTable::try_new(schema?, vec![record_batches])
                .expect("mem table create failed")
                .with_sort_order(vec![vec![datafusion::logical_expr::SortExpr::new(
                    datafusion::prelude::col("time"),
                    true,
                    false,
                )]]),
        )
    }
}

impl DB {
    pub fn as_session_context(&self) -> Result<SessionContext, datafusion::error::DataFusionError> {
        use datafusion::prelude::*;
        let config = SessionConfig::new().set_bool("datafusion.catalog.information_schema", true);
        let ctx = SessionContext::new_with_config(config);

        ctx.register_udf(datafusion::logical_expr::ScalarUDF::new_from_impl(
            FftUDF::new(),
        ));
        ctx.register_udf(datafusion::logical_expr::ScalarUDF::new_from_impl(
            FrequencyDomainUDF::new(),
        ));

        self.with_state(|state| {
            for component in state.components.values() {
                let component_metadata = state
                    .component_metadata
                    .get(&component.component_id)
                    .unwrap();
                let component_name = component_metadata
                    .name
                    .to_case(convert_case::Case::Snake)
                    .replace(".", "_");
                let name = component_name.clone();
                if let Some(mem_table) = component.as_mem_table(&component_name) {
                    ctx.register_table(name, Arc::new(mem_table))?;
                }
            }
            Ok::<_, datafusion::error::DataFusionError>(())
        })?;
        Ok(ctx)
    }
    pub async fn insert_views(
        &self,
        _ctx: &mut SessionContext,
    ) -> Result<(), datafusion::error::DataFusionError> {
        // Views are no longer needed without entity grouping
        Ok(())
    }

    pub fn save_archive(&self, path: impl AsRef<Path>, format: ArchiveFormat) -> Result<(), Error> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        self.with_state(|state| {
            for component in state.components.values() {
                let Some(component_metadata) =
                    state.component_metadata.get(&component.component_id)
                else {
                    continue;
                };

                let column_name = component_metadata.name.clone();
                let mut schema = None;
                let record_batches = component
                    .time_series
                    .list
                    .iter()
                    .map(|node| {
                        let record_batch =
                            node.as_record_batch(column_name.clone(), &component.schema);
                        if schema.is_none() {
                            schema = Some(record_batch.schema());
                        }
                        record_batch
                    })
                    .collect::<Vec<_>>();
                let Some(schema) = schema else { continue };

                match format {
                    ArchiveFormat::ArrowIpc => {
                        let file_name = format!("{column_name}.arrow");
                        let file_path = path.join(file_name);
                        let mut file = File::create(file_path)?;
                        let mut writer =
                            arrow::ipc::writer::FileWriter::try_new(&mut file, &schema)?;
                        for record_batch in record_batches {
                            writer.write(&record_batch)?;
                        }
                        writer.finish()?;
                    }
                    #[cfg(feature = "parquet")]
                    ArchiveFormat::Parquet => {
                        let file_name = format!("{column_name}.parquet");
                        let file_path = path.join(file_name);
                        let mut file = File::create(file_path)?;
                        let mut writer =
                            parquet::arrow::ArrowWriter::try_new(&mut file, schema.clone(), None)?;
                        for record_batch in record_batches {
                            writer.write(&record_batch)?;
                        }
                        writer.close()?;
                    }
                    ArchiveFormat::Csv => {
                        let file_name = format!("{column_name}.csv");
                        let file_path = path.join(file_name);
                        let mut file = File::create(file_path)?;
                        let mut writer = arrow::csv::Writer::new(&mut file);
                        for record_batch in record_batches {
                            writer.write(&record_batch)?;
                        }
                    }
                    #[allow(unreachable_patterns)]
                    _ => return Err(Error::UnsupportedArchiveFormat),
                }
            }
            Ok(())
        })
    }
}

fn node_array_ref<P: ArrowPrimitiveType>(
    node: &TimeSeriesNode,
    range: impl RangeBounds<usize>,
    element_size: usize,
) -> ArrayRef {
    let scalar_buffer = node_scalar_buffer::<P>(node, range, element_size);
    let array = PrimitiveArray::<P>::new(scalar_buffer, None);
    Arc::new(array)
}

fn node_scalar_buffer<P: ArrowPrimitiveType>(
    node: &TimeSeriesNode,
    range: impl RangeBounds<usize>,
    element_size: usize,
) -> ScalarBuffer<P::Native> {
    let buffer = node.data.as_arrow_buffer_range(range, element_size);
    let len = buffer.len() / std::mem::size_of::<P::Native>();
    ScalarBuffer::<P::Native>::new(buffer, 0, len)
}

fn node_bool_ref(
    node: &TimeSeriesNode,
    range: impl RangeBounds<usize>,
    element_size: usize,
) -> ArrayRef {
    let buffer = node.data.as_arrow_buffer_range(range, element_size);
    let len = buffer.len();
    let buf = BooleanBuffer::new(buffer, 0, len);
    Arc::new(BooleanArray::new(buf, None))
}

#[pin_project::pin_project]
pub struct ComponentStream {
    stream: Pin<
        Box<
            dyn Stream<Item = Result<RecordBatch, datafusion::error::DataFusionError>>
                + Send
                + Sync,
        >,
    >,
    schema: SchemaRef,
}
