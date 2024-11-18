use std::{
	fmt::{Display, Formatter},
	sync::mpsc::channel,
};

use bevy::{
	prelude::*,
	render::{
		extract_resource::ExtractResource,
		render_asset::{RenderAssetUsages, RenderAssets},
		render_resource::{
			encase::private::{WriteInto, Writer},
			BindGroup, BindGroupEntry, BindGroupLayout, BindGroupLayoutEntry, BindingResource, BindingType, Buffer,
			BufferBindingType, BufferDescriptor, BufferInitDescriptor, BufferUsages, Extent3d, Maintain, MapMode,
			ShaderStages, ShaderType, StorageBuffer, StorageTextureAccess, TextureDimension, TextureFormat,
			TextureSampleType, TextureUsages, TextureViewDimension,
		},
		renderer::{RenderContext, RenderDevice, RenderQueue},
		texture::GpuImage,
		Extract, RenderApp,
	},
	utils::HashMap,
};

#[derive(Clone)]
enum ShaderBufferStorage {
	Storage { buffer: Buffer, readonly: bool },
	Uniform(Buffer),
	Texture { image: Handle<Image> },
	StorageTexture { format: TextureFormat, access: StorageTextureAccess, image: Handle<Image> },
}

impl ShaderBufferStorage {
	fn bind_group_entry<'a>(&'a self, binding: u32, gpu_images: &'a RenderAssets<GpuImage>) -> BindGroupEntry<'a> {
		match self {
			ShaderBufferStorage::Storage { buffer, readonly: _ } => {
				BindGroupEntry { binding, resource: buffer.as_entire_binding() }
			}
			ShaderBufferStorage::Uniform(buffer) => BindGroupEntry { binding, resource: buffer.as_entire_binding() },
			ShaderBufferStorage::Texture { image } => {
				let image = gpu_images.get(image).unwrap();
				BindGroupEntry { binding, resource: BindingResource::TextureView(&image.texture_view) }
			}
			ShaderBufferStorage::StorageTexture { image, .. } => {
				let image = gpu_images.get(image).unwrap();
				BindGroupEntry { binding, resource: BindingResource::TextureView(&image.texture_view) }
			}
		}
	}

	fn bind_group_layout_entry_binding_type(&self, access_override: Option<StorageTextureAccess>) -> BindingType {
		match &self {
			ShaderBufferStorage::Storage { buffer: _, readonly } => BindingType::Buffer {
				ty: BufferBindingType::Storage { read_only: *readonly },
				has_dynamic_offset: false,
				min_binding_size: None,
			},
			ShaderBufferStorage::Uniform(_) => {
				BindingType::Buffer { ty: BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: None }
			}
			ShaderBufferStorage::Texture { .. } => BindingType::Texture {
				sample_type: TextureSampleType::Float { filterable: true },
				view_dimension: TextureViewDimension::D2,
				multisampled: false,
			},
			ShaderBufferStorage::StorageTexture { format, access, .. } => BindingType::StorageTexture {
				access: access_override.unwrap_or(*access),
				format: *format,
				view_dimension: TextureViewDimension::D2,
			},
		}
	}

	fn set<T: ShaderType + WriteInto>(&self, data: T, render_queue: &RenderQueue) {
		fn set_buffer<T: ShaderType + WriteInto>(data: T, buffer: &Buffer, render_queue: &RenderQueue) {
			let mut bytes = Vec::new();
			let mut writer = Writer::new(&data, &mut bytes, 0).unwrap();
			data.write_into(&mut writer);
			render_queue.write_buffer(buffer, 0, bytes.as_ref());
		}

		if let ShaderBufferStorage::Storage { buffer, readonly: _ } = &self {
			set_buffer(data, buffer, render_queue);
		} else if let ShaderBufferStorage::Uniform(buffer) = &self {
			set_buffer(data, buffer, render_queue);
		} else {
			panic!("Tried to set data on a buffer that isn't a storage or uniform buffer");
		}
	}

	pub fn delete(&mut self, images: &mut Assets<Image>) {
		match &self {
			ShaderBufferStorage::Storage { buffer, .. } => buffer.destroy(),
			ShaderBufferStorage::Uniform(buffer) => buffer.destroy(),
			ShaderBufferStorage::Texture { image } => {
				images.remove(image);
			}
			ShaderBufferStorage::StorageTexture { image, .. } => {
				images.remove(image);
			}
		}
	}

	pub fn image_handle(&self) -> Option<Handle<Image>> {
		match self {
			ShaderBufferStorage::Texture { image } | ShaderBufferStorage::StorageTexture { image, .. } => Some(image.clone()),
			_ => None,
		}
	}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrontBuffer {
	First,
	Second,
}

#[derive(Clone)]
enum ShaderBufferInfo {
	SingleBound { binding: (u32, u32), storage: ShaderBufferStorage },
	SingleUnbound { storage: ShaderBufferStorage },
	Double { binding: (u32, (u32, u32)), front: FrontBuffer, storage: (ShaderBufferStorage, ShaderBufferStorage) },
}

#[derive(Clone, Copy)]
pub enum Binding {
	SingleBound(u32, u32),
	SingleUnbound,
	Double(u32, (u32, u32)),
}

impl ShaderBufferInfo {
	fn new<F: FnMut() -> ShaderBufferStorage>(binding: Binding, mut make_storage: F) -> Self {
		match binding {
			Binding::SingleBound(group, binding) => Self::SingleBound { binding: (group, binding), storage: make_storage() },
			Binding::SingleUnbound => Self::SingleUnbound { storage: make_storage() },
			Binding::Double(group, bindings) => Self::Double {
				binding: (group, bindings),
				front: FrontBuffer::First,
				storage: (make_storage(), make_storage()),
			},
		}
	}

	fn new_storage_uninit(
		render_device: &RenderDevice, size: u32, usage: BufferUsages, binding: Binding, readonly: bool,
	) -> Self {
		Self::new(binding, || ShaderBufferStorage::Storage {
			buffer: render_device.create_buffer(&BufferDescriptor {
				label: None,
				size: size as u64,
				usage,
				mapped_at_creation: false,
			}),
			readonly,
		})
	}

	fn new_storage_zeroed(
		render_device: &RenderDevice, size: u32, usage: BufferUsages, binding: Binding, readonly: bool,
	) -> Self {
		Self::new(binding, || ShaderBufferStorage::Storage {
			buffer: render_device.create_buffer_with_data(&BufferInitDescriptor {
				label: None,
				contents: &vec![0u8; size as usize],
				usage,
			}),
			readonly,
		})
	}

	fn new_storage_init<T: ShaderType + WriteInto + Default + Clone>(
		render_device: &RenderDevice, render_queue: &RenderQueue, data: T, usage: BufferUsages, binding: Binding,
		readonly: bool,
	) -> Self {
		Self::new(binding, || ShaderBufferStorage::Storage {
			buffer: {
				let mut buffer = StorageBuffer::default();
				buffer.set(data.clone());
				buffer.add_usages(usage);
				buffer.write_buffer(&render_device, &render_queue);
				buffer.buffer().unwrap().clone()
			},
			readonly,
		})
	}

	fn new_uniform_init<T: ShaderType + WriteInto + Default + Clone>(
		render_device: &RenderDevice, render_queue: &RenderQueue, data: T, usage: BufferUsages, binding: Binding,
	) -> Self {
		Self::new(binding, || {
			ShaderBufferStorage::Uniform({
				let mut buffer = StorageBuffer::default();
				buffer.set(data.clone());
				buffer.add_usages(usage);
				buffer.write_buffer(&render_device, &render_queue);
				buffer.buffer().unwrap().clone()
			})
		})
	}

	fn new_write_texture(
		images: &mut Assets<Image>, width: u32, height: u32, format: TextureFormat, fill: &[u8],
		access: StorageTextureAccess, binding: Binding,
	) -> Self {
		Self::new(binding, || {
			let mut image = Image::new_fill(
				Extent3d { width: width, height: height, depth_or_array_layers: 1 },
				TextureDimension::D2,
				fill,
				format,
				RenderAssetUsages::RENDER_WORLD,
			);
			image.texture_descriptor.usage =
				TextureUsages::COPY_DST | TextureUsages::STORAGE_BINDING | TextureUsages::TEXTURE_BINDING;
			let image = images.add(image);
			ShaderBufferStorage::StorageTexture { format, access, image }
		})
	}

	fn new_read_write_texture(
		images: &mut Assets<Image>, width: u32, height: u32, format: TextureFormat, fill: &[u8], read_binding: Binding,
		write_binding: Binding,
	) -> (Self, Self) {
		(
			Self::new(read_binding, || {
				let mut image = Image::new_fill(
					Extent3d { width: width, height: height, depth_or_array_layers: 1 },
					TextureDimension::D2,
					fill,
					format,
					RenderAssetUsages::RENDER_WORLD | RenderAssetUsages::MAIN_WORLD,
				);
				image.texture_descriptor.usage = TextureUsages::COPY_DST | TextureUsages::TEXTURE_BINDING;
				let image = images.add(image);
				ShaderBufferStorage::Texture { image: image }
			}),
			Self::new(write_binding, || {
				let mut image = Image::new_fill(
					Extent3d { width: width, height: height, depth_or_array_layers: 1 },
					TextureDimension::D2,
					fill,
					format,
					RenderAssetUsages::RENDER_WORLD,
				);
				image.texture_descriptor.usage =
					TextureUsages::COPY_SRC | TextureUsages::TEXTURE_BINDING | TextureUsages::STORAGE_BINDING;
				let image = images.add(image);
				ShaderBufferStorage::StorageTexture { format, access: StorageTextureAccess::ReadWrite, image: image }
			}),
		)
	}

	fn bind_group_entries<'a>(&'a self, gpu_images: &'a RenderAssets<GpuImage>) -> Vec<BindGroupEntry<'a>> {
		match self {
			Self::SingleBound { binding: (_, binding), storage } => vec![storage.bind_group_entry(*binding, gpu_images)],
			Self::SingleUnbound { .. } => vec![],
			Self::Double { binding: (_, (binding1, binding2)), storage: (storage1, storage2), front } => {
				let (storage1, storage2) =
					if *front == FrontBuffer::First { (storage2, storage1) } else { (storage1, storage2) };
				vec![storage1.bind_group_entry(*binding1, gpu_images), storage2.bind_group_entry(*binding2, gpu_images)]
			}
		}
	}

	fn bind_group_layout_entry(&self) -> Vec<BindGroupLayoutEntry> {
		match &self {
			&ShaderBufferInfo::SingleBound { binding: (_, binding), storage } => vec![BindGroupLayoutEntry {
				binding: *binding,
				visibility: ShaderStages::COMPUTE,
				ty: storage.bind_group_layout_entry_binding_type(None),
				count: None,
			}],
			ShaderBufferInfo::SingleUnbound { .. } => vec![],
			ShaderBufferInfo::Double { binding: (_, (binding1, binding2)), storage: (storage1, storage2), front } => {
				let (storage1, storage2) =
					if *front == FrontBuffer::First { (storage2, storage1) } else { (storage1, storage2) };
				vec![
					BindGroupLayoutEntry {
						binding: *binding1,
						visibility: ShaderStages::COMPUTE,
						ty: storage1.bind_group_layout_entry_binding_type(Some(StorageTextureAccess::ReadOnly)),
						count: None,
					},
					BindGroupLayoutEntry {
						binding: *binding2,
						visibility: ShaderStages::COMPUTE,
						ty: storage2.bind_group_layout_entry_binding_type(Some(StorageTextureAccess::WriteOnly)),
						count: None,
					},
				]
			}
		}
	}

	fn image_handle(&self) -> Option<Handle<Image>> {
		match &self {
			ShaderBufferInfo::SingleBound { storage, .. } | ShaderBufferInfo::SingleUnbound { storage } => {
				storage.image_handle()
			}
			ShaderBufferInfo::Double { storage: (storage1, storage2), front, .. } => {
				let storage = match front {
					FrontBuffer::First => storage1,
					FrontBuffer::Second => storage2,
				};
				storage.image_handle()
			}
		}
	}

	fn set<T: ShaderType + WriteInto + Clone>(&self, data: T, render_queue: &RenderQueue) {
		match &self {
			ShaderBufferInfo::SingleBound { storage, .. } => storage.set(data, render_queue),
			ShaderBufferInfo::SingleUnbound { storage, .. } => storage.set(data, render_queue),
			ShaderBufferInfo::Double { storage: (storage1, storage2), .. } => {
				storage1.set(data.clone(), render_queue);
				storage2.set(data, render_queue);
			}
		};
	}

	pub fn delete(&mut self, images: &mut Assets<Image>) {
		match self {
			ShaderBufferInfo::SingleBound { storage, .. } | ShaderBufferInfo::SingleUnbound { storage } => {
				storage.delete(images)
			}
			ShaderBufferInfo::Double { storage: (storage1, storage2), .. } => {
				storage1.delete(images);
				storage2.delete(images);
			}
		}
	}
}

#[derive(Resource, Clone, ExtractResource)]
pub struct ShaderBufferSet {
	buffers: HashMap<u32, ShaderBufferInfo>,
	groups: Vec<Vec<u32>>,
	next_id: u32,
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub enum ShaderBufferHandle {
	Bound { group: u32, id: u32 },
	Unbound { id: u32 },
}

impl Display for ShaderBufferHandle {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		match self {
			ShaderBufferHandle::Bound { group, id } => {
				write!(f, "{{ group({}), id({}) }}", group, id)
			}
			ShaderBufferHandle::Unbound { id } => write!(f, "{{ id({}) }}", id),
		}
	}
}

fn bind_group_layout(buffers: &Vec<&ShaderBufferInfo>, device: &RenderDevice) -> BindGroupLayout {
	device.create_bind_group_layout(
		None,
		buffers.iter().flat_map(|buffer| buffer.bind_group_layout_entry()).collect::<Vec<_>>().as_slice(),
	)
}

impl ShaderBufferSet {
	pub fn new() -> Self { Self { buffers: HashMap::new(), groups: Vec::new(), next_id: 0 } }

	pub fn add_storage_uninit(
		&mut self, render_device: &RenderDevice, size: u32, usage: BufferUsages, binding: Binding, readonly: bool,
	) -> ShaderBufferHandle {
		self.store_buffer(binding, ShaderBufferInfo::new_storage_uninit(render_device, size, usage, binding, readonly))
	}

	pub fn add_storage_zeroed(
		&mut self, render_device: &RenderDevice, size: u32, usage: BufferUsages, binding: Binding, readonly: bool,
	) -> ShaderBufferHandle {
		self.store_buffer(binding, ShaderBufferInfo::new_storage_zeroed(render_device, size, usage, binding, readonly))
	}

	pub fn add_storage_init<T: ShaderType + WriteInto + Clone + Default>(
		&mut self, render_device: &RenderDevice, render_queue: &RenderQueue, data: T, usage: BufferUsages,
		binding: Binding, readonly: bool,
	) -> ShaderBufferHandle {
		self.store_buffer(
			binding,
			ShaderBufferInfo::new_storage_init(render_device, render_queue, data, usage, binding, readonly),
		)
	}

	pub fn add_uniform_init<T: ShaderType + WriteInto + Clone + Default>(
		&mut self, render_device: &RenderDevice, render_queue: &RenderQueue, data: T, usage: BufferUsages, binding: Binding,
	) -> ShaderBufferHandle {
		self.store_buffer(binding, ShaderBufferInfo::new_uniform_init(render_device, render_queue, data, usage, binding))
	}

	pub fn add_write_texture(
		&mut self, images: &mut Assets<Image>, width: u32, height: u32, format: TextureFormat, fill: &[u8],
		access: StorageTextureAccess, binding: Binding,
	) -> ShaderBufferHandle {
		self
			.store_buffer(binding, ShaderBufferInfo::new_write_texture(images, width, height, format, fill, access, binding))
	}

	pub fn add_read_write_texture(
		&mut self, images: &mut Assets<Image>, width: u32, height: u32, format: TextureFormat, fill: &[u8],
		read_binding: Binding, write_binding: Binding,
	) -> (ShaderBufferHandle, ShaderBufferHandle) {
		let (read, write) =
			ShaderBufferInfo::new_read_write_texture(images, width, height, format, fill, read_binding, write_binding);
		(self.store_buffer(read_binding, read), self.store_buffer(write_binding, write))
	}

	pub fn bind_groups(&self, device: &RenderDevice, gpu_images: &RenderAssets<GpuImage>) -> Vec<BindGroup> {
		self
			.groups
			.iter()
			.map(|buffer_ids| {
				let buffers = buffer_ids.iter().map(|id| self.buffers.get(id).unwrap()).collect::<Vec<_>>();
				device.create_bind_group(
					None,
					&bind_group_layout(&buffers, &device),
					buffers.iter().flat_map(|buffer| buffer.bind_group_entries(gpu_images)).collect::<Vec<_>>().as_slice(),
				)
			})
			.collect()
	}

	pub fn bind_group_layouts(&self, device: &RenderDevice) -> Vec<BindGroupLayout> {
		self
			.groups
			.iter()
			.map(|buffer_ids| {
				let buffers = buffer_ids.iter().map(|id| self.buffers.get(id).unwrap()).collect::<Vec<_>>();
				bind_group_layout(&buffers, device)
			})
			.collect()
	}

	pub fn delete_buffer(&mut self, handle: ShaderBufferHandle, images: &mut Assets<Image>) {
		let buffer = match handle {
			ShaderBufferHandle::Bound { group, id, .. } => {
				let buffer = self.buffers.remove(&id);
				if let Some(buffers) = self.groups.get_mut(group as usize) {
					if let Some(index) = buffers.iter().position(|buffer_id| *buffer_id == id) {
						buffers.remove(index);
					}
				}
				buffer
			}
			ShaderBufferHandle::Unbound { id } => self.buffers.remove(&id),
		};
		if let Some(mut buffer) = buffer {
			buffer.delete(images);
		}
	}

	pub fn image_handle(&self, handle: ShaderBufferHandle) -> Option<Handle<Image>> {
		if let Some(buffer) = self.get_buffer(handle) {
			buffer.image_handle()
		} else {
			None
		}
	}

	pub fn swap_front_buffer(&mut self, handle: ShaderBufferHandle) {
		let buffer = self.get_mut_buffer(handle);
		let Some(buffer) = buffer else {
			panic!("Attempted to set the front buffer of {}, but it doesn't exist", handle);
		};
		let ShaderBufferInfo::Double { front, .. } = buffer else {
			panic!("Attempt to set the front buffer of {}, which isn't a double buffer", handle);
		};
		*front = match front {
			FrontBuffer::First => FrontBuffer::Second,
			FrontBuffer::Second => FrontBuffer::First,
		}
	}

	pub fn set_buffer<T: ShaderType + WriteInto + Clone>(
		&mut self, handle: ShaderBufferHandle, data: T, render_queue: &RenderQueue,
	) {
		if let Some(buffer) = self.get_buffer(handle) {
			buffer.set(data, render_queue);
		} else {
			panic!("Tried to set data on a non-existent buffer");
		}
	}

	pub fn copy_texture(
		&self, src_handle: ShaderBufferHandle, dst_handle: ShaderBufferHandle, render_context: &mut RenderContext,
		images: &RenderAssets<GpuImage>,
	) {
		if let (Some(src), Some(dst)) = (self.get_buffer(src_handle), self.get_buffer(dst_handle)) {
			if let (Some(src), Some(dst)) = (src.image_handle(), dst.image_handle()) {
				if let (Some(src), Some(dst)) = (images.get(&src), images.get(&dst)) {
					let encoder = render_context.command_encoder();
					encoder.copy_texture_to_texture(
						src.texture.as_image_copy(),
						dst.texture.as_image_copy(),
						Extent3d { width: src.texture.width(), height: src.texture.height(), depth_or_array_layers: 1 },
					);
				} else {
					panic!(
						"Tried to copy from texture {} to texture {} when at least one of them doesn't exist on the GPU",
						src_handle, dst_handle
					);
				}
			} else {
				panic!(
					"Tried to copy from texture {} to texture {} when at least one of them isn't a texture",
					src_handle, dst_handle
				);
			}
		} else {
			panic!(
				"Tried to copy from texture {} to texture {} when at least one of them doesn't exist",
				src_handle, dst_handle
			);
		}
	}

	fn store_buffer(&mut self, binding: Binding, buffer: ShaderBufferInfo) -> ShaderBufferHandle {
		let id = self.next_id;
		self.next_id += 1;
		self.buffers.insert(id, buffer);
		match binding {
			Binding::SingleBound(group, _) | Binding::Double(group, _) => {
				if group as usize >= self.groups.len() {
					self.groups.resize(group as usize + 1, Vec::new())
				}
				self.groups[group as usize].push(id);
				ShaderBufferHandle::Bound { group, id }
			}
			Binding::SingleUnbound => ShaderBufferHandle::Unbound { id },
		}
	}

	fn get_buffer(&self, handle: ShaderBufferHandle) -> Option<ShaderBufferInfo> {
		match handle {
			ShaderBufferHandle::Bound { id, .. } | ShaderBufferHandle::Unbound { id } => self.buffers.get(&id).cloned(),
		}
	}

	fn get_mut_buffer(&mut self, handle: ShaderBufferHandle) -> Option<&mut ShaderBufferInfo> {
		match handle {
			ShaderBufferHandle::Bound { id, .. } | ShaderBufferHandle::Unbound { id } => self.buffers.get_mut(&id),
		}
	}
}

fn extract_resources(mut commands: Commands, buffers: Extract<Option<Res<ShaderBufferSet>>>) {
	if let Some(buffers) = &*buffers {
		commands.insert_resource(ShaderBufferSet::extract_resource(&buffers));
	}
}

#[derive(Resource)]
pub struct ShaderBufferRenderSet {
	copy_buffers: HashMap<ShaderBufferHandle, Buffer>,
}

impl ShaderBufferRenderSet {
	fn new() -> Self { Self { copy_buffers: HashMap::new() } }

	pub fn create_copy_buffer(&mut self, handle: ShaderBufferHandle, buffers: &ShaderBufferSet, device: &RenderDevice) {
		if self.copy_buffers.contains_key(&handle) {
			panic!("Tried to create a copy buffer for {}, which already has one", handle);
		}
		let Some(src) = buffers.get_buffer(handle) else {
			panic!("Tried to create a copy buffer for {}, which does not exist", handle);
		};
		let storage = match &src {
			ShaderBufferInfo::SingleBound { storage, .. } | ShaderBufferInfo::SingleUnbound { storage } => storage,
			_ => panic!("Tried to create a copy buffer for {}, which is a double buffer", handle),
		};
		let ShaderBufferStorage::Storage { buffer: src, .. } = storage else {
			panic!("Tried to create a copy buffer for {}, which is not a storage buffer", handle);
		};
		let dst = ShaderBufferInfo::new_storage_uninit(
			device,
			src.size() as u32,
			BufferUsages::COPY_DST | BufferUsages::MAP_READ,
			Binding::SingleUnbound,
			false,
		);
		let ShaderBufferInfo::SingleUnbound { storage: dst_storage } = dst else {
			panic!("Tried to create a copy buffer for {}, but somehow it ended up not unbound", handle);
		};
		let ShaderBufferStorage::Storage { buffer: dst, .. } = dst_storage else {
			panic!("Tried to create a copy buffer for {}, but somehow it ended up as a non-storage buffer", handle);
		};
		self.copy_buffers.insert(handle, dst);
	}

	pub fn remove_copy_buffer(&mut self, handle: ShaderBufferHandle) {
		let Some(buffer) = self.copy_buffers.get(&handle) else {
			panic!("Tried to remove copy buffer for {}, but it doesn't have one", handle);
		};
		buffer.destroy();
		self.copy_buffers.remove(&handle);
	}

	pub fn copy_to_copy_buffer(
		&self, handle: ShaderBufferHandle, buffers: &ShaderBufferSet, context: &mut RenderContext,
	) {
		let Some(src) = buffers.get_buffer(handle) else {
			panic!("Tried to copy from buffer {}, which doesn't exist", handle);
		};
		let src_storage = match &src {
			ShaderBufferInfo::SingleBound { storage, .. } | ShaderBufferInfo::SingleUnbound { storage } => storage,
			_ => panic!("Tried to copy from buffer {}, which is a double buffer", handle),
		};
		let ShaderBufferStorage::Storage { buffer: src, .. } = src_storage else {
			panic!("Tried to copy from buffer {}, which is not a storage buffer", handle);
		};
		let Some(dst) = self.copy_buffers.get(&handle) else {
			panic!("Tried to copy {} to it's copy buffer, but it doesn't yet have one", handle);
		};
		let encoder = context.command_encoder();
		encoder.copy_buffer_to_buffer(&src, 0, &dst, 0, src.size());
	}

	pub fn copy_from_copy_buffer_to_vec(&self, handle: ShaderBufferHandle, device: &RenderDevice) -> Vec<u8> {
		if let Some(buffer) = self.copy_buffers.get(&handle) {
			let buffer_slice = buffer.slice(..);
			let (sender, receiver) = channel();
			buffer_slice.map_async(MapMode::Read, move |result| {
				sender.send(result).unwrap();
			});
			device.poll(Maintain::Wait);
			receiver.recv().unwrap().unwrap();
			let result = buffer_slice.get_mapped_range().to_vec();
			buffer.unmap();
			result
		} else {
			panic!("Tried to copy from buffer {} to vec when it has not yet been copied to a copy buffer", handle);
		}
	}
}

pub struct ShaderBufferSetPlugin;

impl Plugin for ShaderBufferSetPlugin {
	fn build(&self, app: &mut App) {
		app.insert_resource(ShaderBufferSet::new());
		app
			.sub_app_mut(RenderApp)
			.add_systems(ExtractSchedule, extract_resources)
			.insert_resource(ShaderBufferRenderSet::new());
	}
}
