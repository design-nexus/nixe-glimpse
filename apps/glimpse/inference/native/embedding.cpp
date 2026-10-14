// Glimpse-owned ABI over the public llama.cpp/mtmd APIs. No Recall code.
#include "llama.h"
#include "mtmd.h"
#include "mtmd-helper.h"
#include "ggml-backend.h"
#include <cmath>
#include <cstring>
#include <memory>
#include <string>
#include <vector>
#include <algorithm>
#include <thread>

struct Engine {
    llama_model *model=nullptr;
    llama_context *context=nullptr;
    mtmd_context *media=nullptr;
    std::vector<ggml_backend_dev_t> devices;
    ~Engine(){if(media)mtmd_free(media);if(context)llama_free(context);if(model)llama_model_free(model);}
};
static void error(char *destination,size_t capacity,const char *message){if(capacity){std::strncpy(destination,message,capacity-1);destination[capacity-1]=0;}}
static void quiet(ggml_log_level,const char *,void *){}
extern "C" Engine *glimpse_open(const char *text,const char *projector,const char *backends,const char *device,char *err,size_t capacity){
    llama_log_set(quiet,nullptr);mtmd_log_set(quiet,nullptr);mtmd_helper_log_set(quiet,nullptr);
    ggml_backend_load_all_from_path(backends);llama_backend_init();
    auto engine=std::make_unique<Engine>();
    const bool gpu=std::strcmp(device,"cpu")!=0;
    ggml_backend_dev_t selected=nullptr;
    if(gpu){for(size_t i=0;i<ggml_backend_dev_count();++i){auto candidate=ggml_backend_dev_get(i);const char *name=ggml_backend_reg_name(ggml_backend_dev_backend_reg(candidate));if((std::strcmp(device,"cuda")==0&&std::string(name).find("CUDA")!=std::string::npos)||(std::strcmp(device,"vulkan")==0&&std::string(name).find("Vulkan")!=std::string::npos)){selected=candidate;break;}}
        if(!selected){error(err,capacity,"Requested GPU backend is unavailable");return nullptr;}
        engine->devices={selected,nullptr};
    }
    auto model_parameters=llama_model_default_params();model_parameters.n_gpu_layers=gpu?-1:0;if(gpu){model_parameters.devices=engine->devices.data();}
    engine->model=llama_model_load_from_file(text,model_parameters);
    if(!engine->model){error(err,capacity,"Cannot load text model");return nullptr;}
    auto parameters=llama_context_default_params();parameters.n_ctx=2048;parameters.n_batch=2048;parameters.n_ubatch=2048;
    parameters.n_threads=std::min(8u,std::max(1u,std::thread::hardware_concurrency()));parameters.n_threads_batch=parameters.n_threads;
    parameters.embeddings=true;parameters.pooling_type=LLAMA_POOLING_TYPE_MEAN;parameters.attention_type=LLAMA_ATTENTION_TYPE_NON_CAUSAL;
    engine->context=llama_init_from_model(engine->model,parameters);
    if(!engine->context){error(err,capacity,"Cannot create embedding context");return nullptr;}
    if(projector && projector[0]){
        auto media_parameters=mtmd_context_params_default();media_parameters.use_gpu=gpu;media_parameters.device=selected;media_parameters.n_threads=parameters.n_threads;media_parameters.warmup=false;
        media_parameters.image_min_tokens=140;media_parameters.image_max_tokens=280;
        engine->media=mtmd_init_from_file(projector,engine->model,media_parameters);
        if(!engine->media){error(err,capacity,"Cannot load image/audio projector");return nullptr;}
    }
    return engine.release();
}
extern "C" void glimpse_close(Engine *engine){delete engine;}
static bool normalized(Engine *engine,float *out){
    const float *embedding=llama_get_embeddings_seq(engine->context,0);
    if(!embedding || llama_model_n_embd_out(engine->model)!=768)return false;
    double norm=0;for(int i=0;i<768;++i){if(!std::isfinite(embedding[i]))return false;norm+=double(embedding[i])*embedding[i];}
    if(norm<=1e-20){return false;}
    norm=std::sqrt(norm);
    for(int i=0;i<768;++i){out[i]=float(embedding[i]/norm);}
    return true;
}
extern "C" int glimpse_text(Engine *engine,const char *text,float *out,char *err,size_t capacity){
    const auto *vocabulary=llama_model_get_vocab(engine->model);
    int count=llama_tokenize(vocabulary,text,int(std::strlen(text)),nullptr,0,true,true);
    if(count>=0 || -count>2048){error(err,capacity,"Text exceeds the 2048-token embedding window");return 1;}
    std::vector<llama_token> tokens(-count);
    count=llama_tokenize(vocabulary,text,int(std::strlen(text)),tokens.data(),int(tokens.size()),true,true);
    if(count<=0){error(err,capacity,"Tokenization failed");return 1;}
    llama_memory_clear(llama_get_memory(engine->context),true);
    llama_batch batch=llama_batch_init(count,0,1);batch.n_tokens=count;
    for(int i=0;i<count;++i){batch.token[i]=tokens[i];batch.pos[i]=i;batch.n_seq_id[i]=1;batch.seq_id[i][0]=0;batch.logits[i]=1;}
    const int result=llama_decode(engine->context,batch);llama_batch_free(batch);
    if(result || !normalized(engine,out)){error(err,capacity,"Text embedding failed");return 1;}return 0;
}
extern "C" int glimpse_media(Engine *engine,const char *path,float *out,char *err,size_t capacity){
    if(!engine->media){error(err,capacity,"Media model not loaded");return 1;}
    auto bitmap=mtmd_helper_bitmap_init_from_file(engine->media,path,false,mtmd_helper_init_opt_default());
    if(!bitmap.bitmap || bitmap.video_ctx){if(bitmap.bitmap)mtmd_bitmap_free(bitmap.bitmap);if(bitmap.video_ctx)mtmd_helper_video_free(bitmap.video_ctx);error(err,capacity,"Unsupported image/audio input");return 1;}
    std::unique_ptr<mtmd_bitmap,decltype(&mtmd_bitmap_free)> image(bitmap.bitmap,mtmd_bitmap_free);
    std::unique_ptr<mtmd_input_chunks,decltype(&mtmd_input_chunks_free)> chunks(mtmd_input_chunks_init(),mtmd_input_chunks_free);
    const std::string prompt=std::string(mtmd_get_marker(engine->media));
    mtmd_input_text text{prompt.c_str(),prompt.size(),true,true};const mtmd_bitmap *images[]={image.get()};
    if(mtmd_tokenize(engine->media,chunks.get(),&text,images,1)){error(err,capacity,"Media tokenization failed");return 1;}
    size_t total=mtmd_helper_get_n_tokens(chunks.get());if(total>2048){error(err,capacity,"Media exceeds the embedding window");return 1;}
    llama_memory_clear(llama_get_memory(engine->context),true);
    std::unique_ptr<llama_batch_ext,decltype(&llama_batch_ext_free)> batch(llama_batch_ext_init(engine->context),llama_batch_ext_free);
    std::vector<std::vector<float>> buffers;int position=0;
    for(size_t c=0;c<mtmd_input_chunks_size(chunks.get());++c){
        const auto *chunk=mtmd_input_chunks_get(chunks.get(),c);const size_t size=mtmd_input_chunk_get_n_tokens(chunk);
        if(mtmd_input_chunk_get_type(chunk)==MTMD_INPUT_CHUNK_TYPE_TEXT){
            size_t token_count=0;const auto *tokens=mtmd_input_chunk_get_tokens_text(chunk,&token_count);
            for(size_t t=0;t<token_count;++t){int index=llama_batch_ext_add_token(batch.get(),0,tokens[t]);llama_pos pos=position++;if(index<0||!llama_batch_ext_set_pos(batch.get(),index,&pos)||!llama_batch_ext_set_output_embd(batch.get(),index,true)){error(err,capacity,"Invalid text batch");return 1;}}
        }else{
            if(mtmd_encode_chunk(engine->media,chunk)){error(err,capacity,"Media encoding failed");return 1;}
            const size_t width=llama_model_n_embd_inp(engine->model);const float *data=mtmd_get_output_embd(engine->media);
            if(!data){error(err,capacity,"Missing media embedding");return 1;}
            buffers.emplace_back(data,data+size*width);const auto &buffer=buffers.back();
            for(size_t t=0;t<size;++t){llama_embd embd{buffer.data()+t*width,1,width};int index=llama_batch_ext_add_embd(batch.get(),0,embd);llama_pos pos=position++;if(index<0||!llama_batch_ext_set_pos(batch.get(),index,&pos)||!llama_batch_ext_set_output_embd(batch.get(),index,true)){error(err,capacity,"Invalid media batch");return 1;}}
        }
    }
    if(llama_process(engine->context,LLAMA_PROCESS_TYPE_DECODE,batch.get())||!normalized(engine,out)){error(err,capacity,"Multimodal pooling failed");return 1;}return 0;
}
